use prelude::*;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use vec_map::VecMap;

const INITIAL_AUTO_INCREMENT: i32 = 1;

/// Base is used to represent the root nodes of the distributary data flow graph.
///
/// These nodes perform no computation, and their job is merely to persist all received updates and
/// forward them to interested downstream operators. A base node should only be sent updates of the
/// type corresponding to the node's type.
#[derive(Debug, Serialize, Deserialize)]
pub struct Base {
    primary_key: Option<Vec<usize>>,

    defaults: Vec<DataType>,
    dropped: Vec<usize>,
    unmodified: bool,

    // column_index -> last used auto increment value (starts out as None):
    auto_increment_values: HashMap<usize, DataType>,
}

impl Base {
    /// Create a non-durable base node operator.
    pub fn new(defaults: Vec<DataType>) -> Self {
        let mut base = Base::default();
        base.defaults = defaults;
        base
    }

    /// Builder with a known primary key.
    pub fn with_key(mut self, primary_key: Vec<usize>) -> Base {
        self.primary_key = Some(primary_key);
        self
    }

    pub fn with_auto_increment(mut self, column_indices: Vec<usize>) -> Base {
        self.auto_increment_values = column_indices
            .into_iter()
            .map(|index| (index, DataType::None))
            .collect();
        self
    }

    pub fn key(&self) -> Option<&[usize]> {
        self.primary_key.as_ref().map(|cols| &cols[..])
    }

    /// Add a new column to this base node.
    pub fn add_column(&mut self, default: DataType) -> usize {
        assert!(
            !self.defaults.is_empty(),
            "cannot add columns to base nodes without\
             setting default values for initial columns"
        );
        self.defaults.push(default);
        self.unmodified = false;
        self.defaults.len() - 1
    }

    /// Drop a column from this base node.
    pub fn drop_column(&mut self, column: usize) {
        assert!(
            !self.defaults.is_empty(),
            "cannot add columns to base nodes without\
             setting default values for initial columns"
        );
        assert!(column < self.defaults.len());
        self.unmodified = false;

        // note that we don't need to *do* anything for dropped columns when we receive records.
        // the only thing that matters is that new Mutators remember to inject default values for
        // dropped columns.
        self.dropped.push(column);
    }

    pub fn get_dropped(&self) -> VecMap<DataType> {
        self.dropped
            .iter()
            .map(|&col| (col, self.defaults[col].clone()))
            .collect()
    }

    pub fn fix(&self, row: &mut Record) {
        if self.unmodified {
            return;
        }

        if row.len() != self.defaults.len() {
            let rlen = row.len();
            row.extend(self.defaults.iter().skip(rlen).cloned());
        }
    }

    // Replaces 0 values with the next available auto increment value, mutating
    // self.auto_increment_values if there are any changes.
    fn replace_with_auto_increment(&mut self, row: &mut Vec<DataType>) {
        for (index, value) in self.auto_increment_values.iter_mut() {
            // When we're given a 0 value, replace it with the incremented version
            // of the last auto increment value for that column (or the initial increment value):
            *value = match (&row[*index], &value) {
                (&DataType::Int(0), &&mut DataType::None) => DataType::Int(INITIAL_AUTO_INCREMENT),
                (&DataType::BigInt(0), &&mut DataType::None) => {
                    DataType::BigInt(INITIAL_AUTO_INCREMENT as i64)
                }
                (&DataType::Int(0), &&mut DataType::Int(i)) => DataType::Int(i + 1),
                (&DataType::BigInt(0), &&mut DataType::BigInt(i)) => DataType::BigInt(i + 1),
                // Non-0 values should override existing auto increment values, so that
                // the auto incrementer continues from there the next time:
                (&DataType::Int(i), _) => DataType::Int(i),
                (&DataType::BigInt(i), _) => DataType::BigInt(i),
                _ => panic!("tried giving a non-numeric value to a previously numeric column"),
            };

            if *value != row[*index] {
                row[*index] = value.clone();
            }
        }
    }
}

/// A Base clone must have a different unique_id so that no two copies write to the same file.
/// Resetting the writer to None in the original copy is not enough to guarantee that, as the
/// original object can still re-open the log file on-demand from Base::persist_to_log.
impl Clone for Base {
    fn clone(&self) -> Base {
        Base {
            primary_key: self.primary_key.clone(),

            defaults: self.defaults.clone(),
            auto_increment_values: self.auto_increment_values.clone(),
            dropped: self.dropped.clone(),
            unmodified: self.unmodified,
        }
    }
}

impl Default for Base {
    fn default() -> Self {
        Base {
            primary_key: None,

            defaults: Vec::new(),
            auto_increment_values: HashMap::new(),
            dropped: Vec::new(),
            unmodified: true,
        }
    }
}

fn key_val(i: usize, col: usize, r: &Record) -> &DataType {
    match *r {
        Record::Positive(ref r) | Record::Negative(ref r) => &r[col],
        Record::BaseOperation(BaseOperation::Delete { ref key }) => &key[i],
        Record::BaseOperation(BaseOperation::Update { ref key, .. }) => &key[i],
        Record::BaseOperation(BaseOperation::InsertOrUpdate { ref row, .. }) => &row[col],
    }
}

fn key_of<'a>(key_cols: &'a [usize], r: &'a Record) -> impl Iterator<Item = &'a DataType> {
    key_cols
        .iter()
        .enumerate()
        .map(move |(i, col)| key_val(i, *col, r))
}

impl Base {
    pub(crate) fn take(&mut self) -> Self {
        Clone::clone(self)
    }

    pub(crate) fn process(
        &mut self,
        us: LocalNodeIndex,
        mut rs: Records,
        state: &StateMap,
    ) -> Records {
        if self.primary_key.is_none() || rs.is_empty() {
            for r in &mut *rs {
                if let Record::Positive(ref mut u) = r {
                    self.replace_with_auto_increment(u)
                }

                self.fix(r);
            }

            return rs;
        }

        // TODO: try removing this .clone:
        let key_cols = &self.primary_key.as_ref().unwrap().clone()[..];

        let mut rs: Vec<_> = rs.into();
        rs.sort_by(|a, b| key_of(key_cols, a).cmp(key_of(key_cols, b)));

        // starting key
        let mut this_key: Vec<_> = key_of(key_cols, &rs[0]).cloned().collect();

        // starting record state
        let db = state
            .get(&us)
            .expect("base with primary key must be materialized");

        let get_current = |current_key: &'_ _| {
            match db.lookup(key_cols, &KeyType::from(current_key)) {
                LookupResult::Some(rows) => {
                    match rows.len() {
                        0 => None,
                        1 => rows.into_iter().next(),
                        n => {
                            // primary key, so better be unique!
                            assert_eq!(n, 1, "key {:?} not unique (n = {})!", current_key, n);
                            unreachable!();
                        }
                    }
                }
                LookupResult::Missing => unreachable!(),
            }
        };
        let mut current = get_current(&this_key);
        let mut was = current.clone();

        let mut results = Vec::with_capacity(rs.len());
        for r in rs {
            if this_key.iter().cmp(key_of(key_cols, &r)) != Ordering::Equal {
                if current != was {
                    if let Some(was) = was {
                        results.push(Record::Negative(was.into_owned()));
                    }
                    if let Some(current) = current {
                        results.push(Record::Positive(current.into_owned()));
                    }
                }

                this_key = key_of(key_cols, &r).cloned().collect();
                current = get_current(&this_key);
                was = current.clone();
            }

            match r {
                Record::Positive(u) => {
                    if let Some(ref was) = was {
                        eprintln!("base ignoring {:?} since it already has {:?}", u, was);
                    } else {
                        //assert!(was.is_none());
                        current = Some(Cow::Owned(u));
                    }
                }
                Record::Negative(u) => {
                    assert_eq!(current, Some(Cow::from(&u[..])));
                    if current == was {
                        // save us a clone in a common case
                        was = Some(Cow::Owned(u));
                    }
                    current = None;
                }
                Record::BaseOperation(op) => {
                    let update = match op {
                        BaseOperation::Delete { .. } => {
                            if current.is_some() {
                                current = None;
                            } else {
                                // supposed to delete a non-existing row?
                                // TODO: warn?
                            }
                            continue;
                        }
                        BaseOperation::Update { set, .. } => set,
                        BaseOperation::InsertOrUpdate { row, update } => {
                            if current.is_none() {
                                current = Some(Cow::Owned(row));
                                continue;
                            }
                            update
                        }
                    };

                    if current.is_none() {
                        // supposed to update a non-existing row?
                        // TODO: also warn here?
                        continue;
                    }

                    let mut future = current.unwrap().into_owned();
                    for (col, op) in update.into_iter().enumerate() {
                        // XXX: make sure user doesn't update primary key?
                        match op {
                            Modification::Set(v) => future[col] = v,
                            Modification::Apply(op, v) => {
                                let old: i64 = future[col].clone().into();
                                let delta: i64 = v.into();
                                future[col] = match op {
                                    Operation::Add => (old + delta).into(),
                                    Operation::Sub => (old - delta).into(),
                                };
                            }
                            Modification::None => {}
                        }
                    }
                    current = Some(Cow::Owned(future));
                }
            }
        }

        // we may have changed things in the last iteration of the loop above
        if current != was {
            if let Some(was) = was {
                results.push(Record::Negative(was.into_owned()));
            }
            if let Some(current) = current {
                results.push(Record::Positive(current.into_owned()));
            }
        }

        for r in &mut results {
            if let Record::Positive(ref mut u) = r {
                self.replace_with_auto_increment(u)
            }

            self.fix(r);
        }

        results.into()
    }

    pub(crate) fn suggest_indexes(&self, n: NodeIndex) -> HashMap<NodeIndex, (Vec<usize>, bool)> {
        if self.primary_key.is_some() {
            Some((n, (self.primary_key.as_ref().unwrap().clone(), true)))
                .into_iter()
                .collect()
        } else {
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use node;

    struct TestBase {
        base: Base,
        local_addr: LocalNodeIndex,
        states: StateMap,
    }

    fn setup(auto_increment_columns: Vec<usize>) -> TestBase {
        let mut base = Base::new(vec![DataType::None]).with_auto_increment(auto_increment_columns);
        base.add_column("default".into());
        let local_addr = unsafe { LocalNodeIndex::make(0) };
        let mut states = StateMap::default();
        let mut state = box MemoryState::default();
        state.add_key(&[0], None);
        states.insert(local_addr, state);
        TestBase {
            base,
            local_addr,
            states,
        }
    }

    fn one_base_row<R: Into<Record>>(test: &mut TestBase, r: R) -> Records {
        let rs: Records = r.into().into();
        let mut records = test.base.process(test.local_addr, rs.into(), &test.states);
        node::materialize(&mut records, None, test.states.get_mut(&test.local_addr));
        records
    }

    #[test]
    fn it_works_default() {
        let b = Base::default();

        assert!(b.primary_key.is_none());

        assert_eq!(b.defaults.len(), 0);
        assert_eq!(b.dropped.len(), 0);
        assert_eq!(b.unmodified, true);
    }

    #[test]
    fn it_works_new() {
        let b = Base::new(vec![]);

        assert!(b.primary_key.is_none());

        assert_eq!(b.defaults.len(), 0);
        assert_eq!(b.dropped.len(), 0);
        assert_eq!(b.unmodified, true);
    }

    #[test]
    fn it_forwards() {
        let mut base = setup(vec![]);
        let rs: Vec<DataType> = vec![1.into(), "a".into()];
        assert_eq!(one_base_row(&mut base, rs.clone()), vec![rs].into());
    }

    #[test]
    fn it_adds_defaults() {
        let mut base = setup(vec![]);
        let rs: Vec<DataType> = vec![1.into()];
        assert_eq!(
            one_base_row(&mut base, rs.clone()),
            vec![vec![1.into(), "default".into()]].into()
        );
    }

    fn test_lots_of_changes_in_same_batch(mut state: Box<State>) {
        use node;
        use prelude::*;
        use std::collections::HashMap;

        // most of this is from MockGraph
        let mut graph = Graph::new();
        let source = graph.add_node(Node::new(
            "source",
            &["because-type-inference"],
            node::NodeType::Source,
            true,
        ));

        let b = Base::new(vec![]).with_key(vec![0, 2]);
        let global = graph.add_node(Node::new("b", &["x", "y", "z"], b, false));
        graph.add_edge(source, global, ());
        let local = unsafe { LocalNodeIndex::make(0 as u32) };
        let mut ip: IndexPair = global.into();
        ip.set_local(local);
        graph
            .node_weight_mut(global)
            .unwrap()
            .set_finalized_addr(ip);

        let mut remap = HashMap::new();
        remap.insert(global, ip);
        graph.node_weight_mut(global).unwrap().on_commit(&remap);
        graph.node_weight_mut(global).unwrap().add_to(0.into());

        for (_, (col, _)) in graph[global].suggest_indexes(global) {
            state.add_key(&col[..], None);
        }

        let mut states = StateMap::new();
        states.insert(local, state);
        let n = graph[global].take();
        let mut n = n.finalize(&graph);

        let mut one = move |u: Vec<Record>| {
            let mut m = n.get_base_mut().unwrap().process(local, u.into(), &states);
            node::materialize(&mut m, None, states.get_mut(&local));
            m
        };

        assert_eq!(
            one(vec![
                Record::Positive(vec![1.into(), "a".into(), 1.into()]),
                Record::Positive(vec![2.into(), "2a".into(), 1.into()]),
                Record::Negative(vec![1.into(), "a".into(), 1.into()]),
                Record::Positive(vec![1.into(), "b".into(), 1.into()]),
                Record::BaseOperation(BaseOperation::Delete {
                    key: vec![1.into(), 1.into()],
                }),
                Record::BaseOperation(BaseOperation::InsertOrUpdate {
                    row: vec![1.into(), "c".into(), 1.into()],
                    update: vec![
                        Modification::None,
                        Modification::Set("never".into()),
                        Modification::None,
                    ],
                }),
                Record::BaseOperation(BaseOperation::InsertOrUpdate {
                    row: vec![1.into(), "also never".into(), 1.into()],
                    update: vec![
                        Modification::None,
                        Modification::Set("d".into()),
                        Modification::None,
                    ],
                }),
                Record::BaseOperation(BaseOperation::Update {
                    key: vec![1.into(), 1.into()],
                    set: vec![
                        Modification::None,
                        Modification::Set("e".into()),
                        Modification::None,
                    ],
                }),
                Record::BaseOperation(BaseOperation::Update {
                    key: vec![2.into(), 1.into()],
                    set: vec![
                        Modification::None,
                        Modification::Set("2x".into()),
                        Modification::None,
                    ],
                }),
                Record::Negative(vec![1.into(), "e".into(), 1.into()]),
                Record::Negative(vec![2.into(), "2x".into(), 1.into()]),
            ]),
            Records::default()
        );
    }

    #[test]
    fn lots_of_changes_in_same_batch() {
        let state = MemoryState::default();
        test_lots_of_changes_in_same_batch(box state);
    }

    #[test]
    fn lots_of_changes_in_same_batch_persistent() {
        let state = PersistentState::new(
            String::from("lots_of_changes_in_same_batch_persistent"),
            None,
            &PersistenceParameters::default(),
        );

        test_lots_of_changes_in_same_batch(box state);
    }

    #[test]
    fn it_supports_auto_increment_columns() {
        let mut base = setup(vec![0]);
        let strings = vec!["a", "b", "c"];
        for (i, string) in strings.into_iter().enumerate() {
            let rs: Vec<DataType> = vec![0.into(), string.into()];
            assert_eq!(
                one_base_row(&mut base, rs),
                vec![vec![(i + 1).into(), string.into()]].into()
            );
        }

        // Non-0 values should not be overriden by auto increment:
        let excempt: Vec<DataType> = vec![10.into(), "d".into()];
        assert_eq!(
            one_base_row(&mut base, excempt.clone()),
            vec![excempt].into()
        );

        // And the auto increment should then start from that value:
        let regular: Vec<DataType> = vec![0.into(), "e".into()];
        assert_eq!(
            one_base_row(&mut base, regular),
            vec![vec![11.into(), "e".into()]].into()
        );
    }
}
