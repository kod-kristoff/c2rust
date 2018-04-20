//! # Control Flow Graph analysis
//!
//! Through `switch`/`case`/`default` and labels/`goto`, the C language supports jumping directly
//! from one position in the code to another. Rust supports on structured control flow constructs.
//! This means that during translation, we need to somehow eliminate the unstructured control-flow
//! constructs C has. This module is where that happens.
//!
//! In a nutshell, here are the steps:
//!
//!   - given an entry point C statement, translate it into a CFG consisting of `BasicBlock<Label>`
//!   - simplify this CFG (by eliminating empty blocks that jump unconditionally to the next block)
//!   - use the _Relooper algorithm_ to convert this CFG into a sequence of `Structure<StmtOrDecl>`s
//!   - place the declarations in the right place and produce a sequence of `Structure<Stmt>`s
//!   - simplify that sequence of `Structure<Stmt>`s into another such sequence
//!   - convert the `Vec<Structure<Stmt>>` back into a `Vec<Stmt>`
//!

use syntax;
use syntax::ast::*;
use syntax::ptr::P;
use idiomize::ast_manip::make_ast::*;
use std::collections::{HashSet, HashMap};
use c_ast::CLabelId;
use std::ops::Index;
use syntax::print::pprust;
use std::io;
use std::fs::File;
use std::io::Write;
use std::ops::Deref;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::hash::Hash;

use translator::*;
use c_ast::*;

pub mod relooper;
pub mod structures;

/// These labels identify basic blocks in a regular CFG.
#[derive(Copy,Clone,PartialEq,Eq,PartialOrd,Ord,Debug,Hash)]
pub enum Label {
    /// Some labels come directly from the C side (namely those created from labels, cases, and
    /// defaults). For those, we just re-use the `CLabelId` of the C AST node.
    FromC(CLabelId),

    /// Most labels are synthetically created while unwrapping control-flow constructs (like loops)
    /// into basic blocks.
    Synthetic(u64),
}

impl Label {
    fn pretty_print(&self) -> String {
        match self {
            &Label::FromC(CStmtId(label_id)) => format!("'c_{}", label_id),
            &Label::Synthetic(syn_id) => format!("'s_{}", syn_id),
        }
    }

    fn debug_print(&self) -> String {
        String::from(self.pretty_print().trim_left_matches('\''))
    }

    fn to_num_expr(&self) -> P<Expr> {
        let mut s = DefaultHasher::new();
        self.hash(&mut s);
        let as_num = s.finish();

        mk().lit_expr(mk().int_lit(as_num as u128, ""))
    }

    fn to_string_expr(&self) -> P<Expr> {
        mk().lit_expr(mk().str_lit(self.debug_print()))
    }
}

/// These labels identify _structure_ basic blocks in a structure CFG.
#[derive(Clone,Debug)]
pub enum StructureLabel<S> {
    GoTo(Label),
    ExitTo(Label),
    Nested(Vec<Structure<S>>),
}

impl StructureLabel<StmtOrDecl> {

    /// Produce a new `StructureLabel` from the existing one by replacing all `StmtOrDecl::Decl`
    /// variants with either a declaration with initializer or only an initializer.
    fn place_decls(
        self,
        lift_me: &HashSet<CDeclId>,
        store: &mut DeclStmtStore,
    ) -> StructureLabel<Stmt> {
        match self {
            StructureLabel::GoTo(l) => StructureLabel::GoTo(l),
            StructureLabel::ExitTo(l) => StructureLabel::ExitTo(l),
            StructureLabel::Nested(vs) => {
                let vs = vs.into_iter().map(|s| s.place_decls(lift_me, store)).collect();
                StructureLabel::Nested(vs)
            }
        }
    }
}

/// These are the things that the relooper algorithm produces.
#[derive(Clone,Debug)]
pub enum Structure<Stmt> {
    /// Series of statements and what to do after
    Simple {
        entries: HashSet<Label>,
        body: Vec<Stmt>,
        terminator: GenTerminator<StructureLabel<Stmt>>,
    },
    /// Looping constructs
    Loop {
        entries: HashSet<Label>,
        body: Vec<Structure<Stmt>>,
    },
    /// Branching constructs??
    Multiple {
        entries: HashSet<Label>,
        branches: HashMap<Label, Vec<Structure<Stmt>>>,
        then: Vec<Structure<Stmt>>,
    }
}

impl<S> Structure<S> {
    fn get_entries(&self) -> &HashSet<Label> {
        match self {
            &Structure::Simple { ref entries, .. } => entries,
            &Structure::Loop { ref entries, .. } => entries,
            &Structure::Multiple { ref entries, .. } => entries,
        }
    }
}

impl Structure<StmtOrDecl> {

    /// Produce a new `Structure` from the existing one by replacing all `StmtOrDecl::Decl`
    /// variants with either a declaration with initializer or only an initializer.
    fn place_decls(self, lift_me: &HashSet<CDeclId>, store: &mut DeclStmtStore) -> Structure<Stmt> {
        match self {
            Structure::Simple { entries, body, terminator } => {
                let mut body = body
                    .into_iter()
                    .flat_map(|s: StmtOrDecl| -> Vec<Stmt> { s.place_decls(lift_me, store) })
                    .collect();
                let terminator = terminator.place_decls(lift_me, store);
                Structure::Simple { entries, body, terminator }
            }
            Structure::Loop { entries, body } => {
                let body = body.into_iter().map(|s| s.place_decls(lift_me, store)).collect();
                Structure::Loop { entries, body }
            }
            Structure::Multiple { entries, branches, then } => {
                let branches = branches
                    .into_iter()
                    .map(|(lbl, vs)|
                        (lbl, vs.into_iter().map(|s| s.place_decls(lift_me, store)).collect())
                    )
                    .collect();
                let then = then.into_iter().map(|s| s.place_decls(lift_me, store)).collect();
                Structure::Multiple { entries, branches, then }
            }
        }
    }
}

/// Generalized basic block.
#[derive(Clone, Debug)]
struct BasicBlock<L,S> {
    /// Jump-free code
    body: Vec<S>,

    /// How to find the next (if any) basic block to go to
    terminator: GenTerminator<L>,

    /// Variables live at the beginning of this block
    live: HashSet<CDeclId>,

    /// Variables defined in this block
    defined: HashSet<CDeclId>,
}

impl<L,S> BasicBlock<L,S> {
    fn new(terminator: GenTerminator<L>) -> Self {
        BasicBlock { body: vec![], terminator, live: HashSet::new(), defined: HashSet::new() }
    }

    fn new_jump(target: L) -> Self {
        BasicBlock::new(Jump(target))
    }
}

impl<S1,S2> BasicBlock<StructureLabel<S1>,S2> {

    /// Get all of the `GoTo` targets of a structure basic block
    fn successors(&self) -> HashSet<Label> {
        self.terminator
            .get_labels()
            .iter()
            .filter_map(|&slbl|
                match slbl {
                    &StructureLabel::GoTo(tgt) => Some(tgt),
                    _ => None,
                }
            )
            .collect()
    }
}

/// Represents the control flow choices one can make when at the end of a `BasicBlock`.
#[derive(Clone, Debug)]
pub enum GenTerminator<Lbl> {
    /// End of control-flow. For example: the last statement in a function, or a return
    End,

    /// Unconditional branch to another block
    Jump(Lbl),

    /// Conditional branch to another block. The expression is expected to be a boolean Rust
    /// expression
    Branch(P<Expr>, Lbl, Lbl),

    /// Multi-way branch. The patterns are expected to match the type of the expression.
    Switch {
        expr: P<Expr>,
        cases: Vec<(Vec<P<Pat>>, Lbl)>, // TODO: support ranges of expressions
    }
}

// We use this a lot, so import its constructors
use self::GenTerminator::*;

impl<L> GenTerminator<L> {

    /// Produce a new terminator by transforming all of the labels in that terminator.
    fn map_labels<F: Fn(&L) -> N, N>(&self, func: F) -> GenTerminator<N> {
        match self {
            &End => End,
            &Jump(ref l) => Jump(func(l)),
            &Branch(ref e, ref l1, ref l2) => Branch(e.clone(), func(l1), func(l2)),
            &Switch { ref expr, ref cases } => Switch {
                expr: expr.clone(),
                cases: cases.iter().map(|&(ref e, ref l)| (e.clone(), func(l))).collect(),
            }
        }
    }

    /// Export references to all of the labels in the terminator
    fn get_labels(&self) -> Vec<&L> {
        match self {
            &End => vec![],
            &Jump(ref l) => vec![l],
            &Branch(_, ref l1, ref l2) => vec![l1,l2],
            &Switch { ref cases, .. } =>
                cases.iter().map(|&(_, ref l)| l).collect(),
        }
    }

    /// Export mutable references to all of the labels in the terminator
    fn get_labels_mut(&mut self) -> Vec<&mut L> {
        match self {
            &mut End => vec![],
            &mut Jump(ref mut l) => vec![l],
            &mut Branch(_, ref mut l1, ref mut l2) => vec![l1,l2],
            &mut Switch { ref mut cases, .. } =>
                cases.iter_mut().map(|&mut (_, ref mut l)| l).collect(),
        }
    }
}

impl GenTerminator<StructureLabel<StmtOrDecl>> {

    /// Produce a new `GenTerminator` from the existing one by replacing all `StmtOrDecl::Decl`
    /// variants with either a declaration with initializer or only an initializer.
    fn place_decls(
        self,
        lift_me: &HashSet<CDeclId>,
        store: &mut DeclStmtStore
    ) -> GenTerminator<StructureLabel<Stmt>> {
        match self {
            End => End,
            Jump(l) => {
                let l = l.place_decls(lift_me, store);
                Jump(l)
            },
            Branch(e, l1, l2) => {
                let l1 = l1.place_decls(lift_me, store);
                let l2 = l2.place_decls(lift_me, store);
                Branch(e, l1, l2)
            },
            Switch { expr, cases } => {
                let cases = cases
                    .into_iter()
                    .map(|(e, l)| (e, l.place_decls(lift_me, store)))
                    .collect();
                Switch { expr, cases }
            }
        }
    }
}

/// The sole purpose of this structure is to accumulate information about what cases/default have
/// been seen which translating the body of the switch.
#[derive(Clone, Debug, Default)]
pub struct SwitchCases {
    cases: Vec<(P<Pat>,Label)>,
    default: Option<Label>,
}

/// A Rust statement, or a C declaration.
#[derive(Clone, Debug)]
pub enum StmtOrDecl {
    /// Rust statement that was translated from a non-compound and non-declaration C statement.
    Stmt(Stmt),

    /// C declaration
    Decl(CDeclId),
}

impl StmtOrDecl {

    /// Produce a `Stmt` by replacing `StmtOrDecl::Decl`  variants with either a declaration with
    /// initializer or only an initializer.
    fn place_decls(self, lift_me: &HashSet<CDeclId>, store: &mut DeclStmtStore) -> Vec<Stmt> {
        match self {
            StmtOrDecl::Stmt(s) => vec![s],
            StmtOrDecl::Decl(d) if lift_me.contains(&d) => store.extract_assign(d).unwrap(),
            StmtOrDecl::Decl(d) => store.extract_decl_and_assign(d).unwrap(),
        }
    }
}

/// A CFG graph of regular basic blocks.
#[derive(Clone, Debug)]
pub struct Cfg<Lbl: Eq + Hash, Stmt> {
    /// Entry point in the graph
    entries: HashSet<Lbl>,

    /// Nodes in the graph
    nodes: HashMap<Lbl, BasicBlock<Lbl,Stmt>>,
}

#[derive(Copy, Clone, Debug)]
enum ExitStyle {
    Continue,
    Break,
}

/// Reaching the end of a body without encountering a `return` means different things depending on
/// the function we are in.
#[derive(Copy, Clone, Debug)]
pub enum ImplicitReturnType {
    /// The `main` function implicitly returns `0`
    Main,

    /// `void` functions implicitly `return;` at the end of their bodies
    Void,

    /// We require that a non-`main` function not returning `void` have an explicit return. C99 is
    /// annoyingly more permissive. From 6.9.1 paragraph 12,
    ///
    /// > If the `}` that terminates a function is reached, and the value of the function call is
    /// > used by the caller, the behavior is undefined."
    NoImplicitReturnType,
}

/// A complete control-flow graph
impl Cfg<Label, StmtOrDecl> {

    /// Completely process a statement into a control flow graph.
    pub fn from_stmt(
        translator: &Translation,
        stmt_id: CStmtId,
        ret: ImplicitReturnType,
    ) -> Result<(Self, DeclStmtStore), String> {

        let mut cfg_builder = CfgBuilder::new();
        let entry = *cfg_builder.graph.entries.iter().next().expect("from_stmt: expected an entry");

        let body_stuff = translator.with_scope(|| {
            let entry_wip = cfg_builder.new_wip_block(entry);
            cfg_builder.convert_stmt_help(translator, stmt_id, entry_wip)
        })?;
        if let Some(WipBlock { label: body_label, mut body, defined, live }) = body_stuff {

            let ret_expr: Option<P<Expr>> = match ret {
                ImplicitReturnType::Main => Some(mk().lit_expr(mk().int_lit(0, ""))),
                ImplicitReturnType::Void => None as Option<P<Expr>>,
                ImplicitReturnType::NoImplicitReturnType => Some(Translation::panic()), // TODO: this could be better
            };
            body.push(StmtOrDecl::Stmt(mk().semi_stmt(mk().return_expr(ret_expr))));

            let body_bb = BasicBlock { body, terminator: End, defined, live };
            cfg_builder.add_block(body_label, body_bb);
        }

        cfg_builder.graph.prune_empty_blocks_mut();
        cfg_builder.graph.prune_unreachable_blocks_mut();

        Ok((cfg_builder.graph, cfg_builder.decls_seen))
    }
}

/// The polymorphism here is only to make it clear exactly how little these functions need to know
/// about the actual contents of the CFG - we only actual call these on one monomorphic CFG type.
impl<Lbl: Copy + Eq + Hash, Stmt> Cfg<Lbl, Stmt> {

    /// Removes blocks that cannot be reached from the CFG
    pub fn prune_unreachable_blocks_mut(&mut self) -> () {
        let visited: HashSet<Lbl> = {
            let mut visited: HashSet<Lbl> = HashSet::new();
            let mut to_visit: Vec<&Lbl> = self.entries.iter().collect();

            while let Some(lbl) = to_visit.pop() {
                if visited.contains(lbl) {
                    continue;
                }

                let blk = self.nodes.get(lbl).expect("prune_unreachable_blocks: block not found");
                visited.insert(*lbl);

                for lbl in &blk.terminator.get_labels() {
                    if !visited.contains(lbl) {
                        to_visit.push(lbl);
                    }
                }
            }

            visited
        };

        self.nodes.retain(|lbl, _| visited.contains(lbl))
    }

    /// Removes empty blocks whose terminator is just a `Jump` by merging them with the block they
    /// are jumping to.
    pub fn prune_empty_blocks_mut(&mut self) -> () {

        // Keys are labels corresponding to empty basic blocks with a jump terminator, values are
        // the labels they jump to (and can hopefully be replaced by).
        let mut proposed_rewrites: HashMap<Lbl, Lbl> = self.nodes
            .iter()
            .filter_map(|(lbl, bb)| Cfg::empty_bb(bb).map(|tgt| (*lbl, tgt)))
            .collect();

        // Rewrites to actually apply. Keys are labels to basic blocks that were remapped into the
        // basic block corresponding to the value.
        let mut actual_rewrites: HashMap<Lbl, Lbl> = HashMap::new();

        while let Some((from, to)) = proposed_rewrites.iter().map(|(f,t)| (*f,*t)).next() {
            proposed_rewrites.remove(&from);
            let mut from_any: HashSet<Lbl> = vec![from].into_iter().collect();

            // Try to apply more rewrites from `proposed_rewrites`
            let mut to_intermediate: Lbl = to;
            while let Some(to_new) = proposed_rewrites.remove(&to_intermediate) {
                from_any.insert(to_intermediate);
                to_intermediate = to_new;
            }

            // Check if there were already some rewrites applied
            let to_final = match actual_rewrites.get(&to_intermediate) {
                None => to_intermediate,
                Some(&to_final) => {
                    from_any.insert(to_intermediate);
                    to_final
                }
            };

            // It makes no sense to remap something to itself
            for from in from_any {
                if from != to_final {
                    actual_rewrites.insert(from, to_final);
                }
            }
        }

        // Apply the remaps to the entries
        self.entries = self.entries
            .iter()
            .map(|entry| *actual_rewrites.get(entry).unwrap_or(entry))
            .collect();

        // We keep only the basic blocks that weren't remapped to anything.
        self.nodes.retain(|lbl, _| actual_rewrites.get(lbl).is_none());

        // However, those block we do keep, we remap the labels in their terminators.
        for bb in self.nodes.values_mut() {
            for lbl in bb.terminator.get_labels_mut() {
                if let Some(new_lbl) = actual_rewrites.get(lbl) {
                    *lbl = *new_lbl;
                }
            }
        }
    }

    /// Given an empty `BasicBlock` that ends in a `Jump`, return the target label. In all other
    /// cases, return `None`.
    fn empty_bb(bb: &BasicBlock<Lbl,Stmt>) -> Option<Lbl> {
        match bb.terminator {
            Jump(lbl) if bb.body.is_empty() => Some(lbl),
            _ => None,
        }
    }
}

/// This stores all of the state required to construct a control-flow graph from C statements. Once
/// the graph is constructed, we only really care about the 'graph' field.
#[derive(Clone, Debug)]
struct CfgBuilder {

    /// Identifies the 'BasicBlock' to start with in 'graph'
    graph: Cfg<Label,StmtOrDecl>,

    /// Source for generating fresh synthetic labels
    prev_label: u64,

    /// Stack of labels identifying what a 'break' should jump to. We push onto this stack when
    /// entering a construct that can break and pop when exiting that construct.
    break_labels: Vec<Label>,
    /// Like 'break_labels', but for 'continue'.
    continue_labels: Vec<Label>,
    /// Accumulates information for the 'case'/'default' encountered so far while translating the
    /// body of a 'switch'.
    switch_expr_cases: Vec<SwitchCases>,

    /// Variables in scope right before the current statement. The wrapping `Vec` witnesses the
    /// notion of scope: later elements in the vector are always supersets of earlier elements.
    currently_live: Vec<HashSet<CDeclId>>,

    /// Information about all of the C declarations we have seen so far.
    decls_seen: DeclStmtStore,
}

/// Stores information about translating C declarations to Rust statements. When seeing a C
/// declaration, we often don't know if it is already in the right place. The fix is to punt: we
/// put into a `DeclStmtStore` information about what to do in all possible cases and we delay
/// choosing what to do until later.
#[derive(Clone, Debug)]
pub struct DeclStmtStore {
    store: HashMap<CDeclId, DeclStmtInfo>
}

/// This contains the information one needs to convert a C declaration in all the possible ways:
///
///  1. declare and initialize
///  2. declare only (and incidentally zero-initialize)
///  3. intialize only (if the declaration has already been emitted)
///
#[derive(Clone, Debug)]
pub struct DeclStmtInfo {
    /// Just the declaration
    pub decl: Option<Vec<Stmt>>,

    /// Just the assignment
    pub assign: Option<Vec<Stmt>>,

    /// Both the declaration and the assignment
    pub decl_and_assign: Option<Vec<Stmt>>,

    /// Statements that need to run to make `assign` and `decl_and_assign` work
    pub pre_init: Option<Vec<Stmt>>,
}

impl DeclStmtInfo {
    pub fn new(
        decl: Vec<Stmt>,
        assign: Vec<Stmt>,
        decl_and_assign: Vec<Stmt>,
        pre_init: Vec<Stmt>
    ) -> Self {
        DeclStmtInfo {
            decl: Some(decl),
            assign: Some(assign),
            decl_and_assign: Some(decl_and_assign),
            pre_init: Some(pre_init),
        }
    }
}

impl DeclStmtStore {

    pub fn new() -> Self {
        DeclStmtStore { store: HashMap::new() }
    }

    /// Export _just_ the Rust statements for a declaration (without initialization). Used when you
    /// want to move just a declaration to a larger scope.
    pub fn extract_decl(&mut self, decl_id: CDeclId) -> Result<Vec<Stmt>, String> {
        let DeclStmtInfo { decl, assign, pre_init, .. } = self.store
            .remove(&decl_id)
            .ok_or(format!("Cannot find information on declaration {:?}", decl_id))?;

        let decl: Vec<Stmt> = decl.ok_or(format!("Declaration for {:?} has already been exported", decl_id))?;

        let pruned = DeclStmtInfo { decl: None, assign, decl_and_assign: None, pre_init };
        self.store.insert(decl_id, pruned);

        Ok(decl)
    }

   /// Export _just_ the Rust statements for an initializer (without the declaration it was
   /// initially attached to). Used when you've moved a declaration but now you need to also run the
   /// initializer.
    pub fn extract_assign(&mut self, decl_id: CDeclId) -> Result<Vec<Stmt>, String> {
        let DeclStmtInfo { decl, assign, pre_init, .. } = self.store
            .remove(&decl_id)
            .ok_or(format!("Cannot find information on declaration {:?}", decl_id))?;

        let pre_init: Vec<Stmt> = pre_init.ok_or(format!("Pre-initializer for {:?} has already been exported", decl_id))?;
        let assign: Vec<Stmt> = assign.ok_or(format!("Assignment for {:?} has already been exported", decl_id))?;

        let pruned = DeclStmtInfo { decl, assign: None, decl_and_assign: None, pre_init: None };
        self.store.insert(decl_id, pruned);

        let mut ret: Vec<Stmt> = vec![];
        ret.extend(&mut pre_init.into_iter());
        ret.extend(&mut assign.into_iter());

        Ok(ret)
    }

    /// Export the Rust statements for the full declaration and initializers. Used for when you
    /// didn't need to move a declaration at all.
    pub fn extract_decl_and_assign(&mut self, decl_id: CDeclId) -> Result<Vec<Stmt>, String> {
        let DeclStmtInfo { decl_and_assign, pre_init, .. } = self.store
            .remove(&decl_id)
            .ok_or(format!("Cannot find information on declaration {:?}", decl_id))?;

        let pre_init: Vec<Stmt> = pre_init.ok_or(format!("Pre-initializer for {:?} has already been exported", decl_id))?;
        let decl_and_assign: Vec<Stmt> = decl_and_assign.ok_or(format!("Declaration with assignment for {:?} has already been exported", decl_id))?;

        let pruned = DeclStmtInfo { decl: None, assign: None, decl_and_assign: None, pre_init: None };
        self.store.insert(decl_id, pruned);

        let mut ret: Vec<Stmt> = vec![];
        ret.extend(&mut pre_init.into_iter());
        ret.extend(&mut decl_and_assign.into_iter());

        Ok(ret)
    }

    /// Export the Rust statements for the full declaration and initializers. DEBUGGING ONLY.
    pub fn peek_decl_and_assign(&self, decl_id: CDeclId) -> Result<Vec<Stmt>, String> {
        let &DeclStmtInfo { ref decl_and_assign, ref pre_init, .. } = self.store
            .get(&decl_id)
            .ok_or(format!("Cannot find information on declaration {:?}", decl_id))?;

        let pre_init: Vec<Stmt> = pre_init.clone().ok_or(format!("Pre-initializer for {:?} has already been exported", decl_id))?;
        let decl_and_assign: Vec<Stmt> = decl_and_assign.clone().ok_or(format!("Declaration with assignment for {:?} has already been exported", decl_id))?;

        let mut ret: Vec<Stmt> = vec![];
        ret.extend(&mut pre_init.into_iter());
        ret.extend(&mut decl_and_assign.into_iter());

        Ok(ret)
    }
}

/// Represents a `BasicBlock` under construction where the bit under construction is the end. Extra
/// statements may be added in the `body` and extra declarations may be added to `defined`. However,
/// the `label` and `live` should not change.
struct WipBlock {
    /// Label of WIP.
    label: Label,

    /// Statements so far in the WIP.
    body: Vec<StmtOrDecl>,

    /// Variables defined so far in this WIP.
    defined: HashSet<CDeclId>,

    /// Variables live in this WIP.
    live: HashSet<CDeclId>,
}

impl Extend<Stmt> for WipBlock {
    fn extend<T: IntoIterator<Item = Stmt>>(&mut self, iter: T) {
        for stmt in iter.into_iter() {
            self.body.push(StmtOrDecl::Stmt(stmt))
        }
    }
}

impl WipBlock {
    pub fn push_stmt(&mut self, stmt: Stmt) {
        self.body.push(StmtOrDecl::Stmt(stmt))
    }

    pub fn push_decl(&mut self, decl: CDeclId) {
        self.body.push(StmtOrDecl::Decl(decl))
    }
}

/// This impl block deals with creating control flow graphs
impl CfgBuilder {

    /// Add a basic block to the control flow graph, specifying under which label to insert it.
    fn add_block(&mut self, lbl: Label, bb: BasicBlock<Label,StmtOrDecl>) -> () {
        let currently_live = self.currently_live
            .last_mut()
            .expect("Found no live currently live scope");

        for decl in &bb.defined {
            currently_live.insert(*decl);
        }

        match self.graph.nodes.insert(lbl, bb) {
            None => { },
            Some(_) => panic!("Label {:?} cannot identify two basic blocks", lbl),
        }
    }

    /// Create a basic block from a WIP block by tacking on the right terminator. Once this is done,
    /// add the block into the graph.
    fn add_wip_block(&mut self, wip: WipBlock, terminator: GenTerminator<Label>) -> () {
        let WipBlock { label, body, defined, live } = wip;
        self.add_block(label, BasicBlock { body, terminator, defined, live });
    }

    /// Update the terminator of an existing block. This is for the special cases where you don't
    /// know the terminators of a block by visiting it.
    fn update_terminator(&mut self, lbl: Label, new_term: GenTerminator<Label>) -> () {
        match self.graph.nodes.get_mut(&lbl) {
            None => panic!("Cannot find label {:?} to update", lbl),
            Some(bb) => bb.terminator = new_term,
        }
    }

    /// REMARK: make sure that basic blocks are constructed either entirely inside or entirely
    ///         outside `with_scope`. Otherwise, the scope of the block is going to be confused.
    fn with_scope<B, F: FnOnce(&mut Self) -> B>(&mut self, translator: &Translation, cont: F) -> B {

        // Open a new scope
        let new_vars = self.current_variables();
        self.currently_live.push(new_vars);

        let b = translator.with_scope(|| cont(self));

        // Close the scope
        self.currently_live
            .pop()
            .expect("Found no live currently live scope to close");

        b
    }

    fn current_variables(&self) -> HashSet<CDeclId> {
        self.currently_live
            .last()
            .expect("Found no live currently live scope")
            .clone()
    }

    // Start a new basic block WIP.
    fn new_wip_block(&mut self, new_label: Label) -> WipBlock {
        WipBlock {
            label: new_label,
            body: vec![],
            defined: HashSet::new(),
            live: self.current_variables(),
        }
    }

    /// Generate a fresh (synthetic) label.
    fn fresh_label(&mut self) -> Label {
        self.prev_label += 1;
        Label::Synthetic(self.prev_label)
    }

    /// Create a new `CfgBuilder` with a single entry label.
    fn new() -> CfgBuilder {
        let entries = vec![Label::Synthetic(0)].into_iter().collect();

        CfgBuilder {
            graph: Cfg {
                entries,
                nodes: HashMap::new(),
            },

            prev_label: 0,

            break_labels: vec![],
            continue_labels: vec![],
            switch_expr_cases: vec![],

            currently_live: vec![HashSet::new()],
            decls_seen: DeclStmtStore::new(),
        }
    }


    /// Translate a C statement and tack it onto the end of the `WipBlock` passed in. If necessary,
    /// intermediate basic blocks can be outputted to the control flow graph.
    ///
    /// If the input C statement naturally passes control to the statement that follows it, the
    /// return should be the new `WipBlock` (and the label by which this WIP block is referred to).
    ///
    /// NOTE: This is the workhorse for generating control flow graphs. By passing threading through
    ///       a WIP block, we can avoid making a lot of small blocks.
    ///
    /// NOTE: It is important that we finish adding a block to the graph before we start creating
    ///       the next one. Every time a new block is started with `new_wip_block`, we take a
    ///       snapshot of the live variables from `currently_live`.
    fn convert_stmt_help(
        &mut self,
        translator: &Translation,
        stmt_id: CStmtId,         // C statement to translate
        mut wip: WipBlock,        // Current WIP block
    ) -> Result<Option<WipBlock>, String> {

        match translator.ast_context.index(stmt_id).kind {
            CStmtKind::Empty => Ok(Some(wip)),

            CStmtKind::Decls(ref decls) => {
                for decl in decls {
                    let info = translator.convert_decl_stmt_info(*decl)?;
                    self.decls_seen.store.insert(*decl, info);

                    wip.push_decl(*decl);
                    wip.defined.insert(*decl);
                }
                Ok(Some(wip))
            }

            CStmtKind::Return(expr) => {
                let val = match expr.map(|i| translator.convert_expr(ExprUse::RValue, i, false)) {
                    Some(r) => Some(r?),
                    None => None,
                };

                let WithStmts { stmts, val: ret_val } = with_stmts_opt(val);
                wip.extend(stmts);
                wip.push_stmt(mk().expr_stmt(mk().return_expr(ret_val)));

                self.add_wip_block(wip, End);

                Ok(None)
            }

            CStmtKind::If { scrutinee, true_variant, false_variant } => {
                let next_entry = self.fresh_label();
                let then_entry = self.fresh_label();
                let else_entry = if false_variant.is_none() { next_entry } else { self.fresh_label() };

                // Condition
                let WithStmts { stmts, val } = translator.convert_condition(true, scrutinee, false)?;
                wip.extend(stmts);
                self.add_wip_block(wip, Branch(val, then_entry, else_entry));

                // Then case
                let then_wip = self.new_wip_block(then_entry);
                let then_stuff = self.convert_stmt_help(translator, true_variant, then_wip)?;
                if let Some(wip_then) = then_stuff {
                    self.add_wip_block(wip_then, Jump(next_entry));
                }

                // Else case
                if let Some(false_var) = false_variant {
                    let else_wip = self.new_wip_block(else_entry);
                    let else_stuff = self.convert_stmt_help(translator, false_var, else_wip)?;
                    if let Some(wip_else) = else_stuff {
                        self.add_wip_block(wip_else, Jump(next_entry));
                    }
                };

                // Return
                Ok(Some(self.new_wip_block(next_entry)))
            }

            CStmtKind::While { condition, body: body_stmt } => {
                let cond_entry = self.fresh_label();
                let body_entry = self.fresh_label();
                let next_entry = self.fresh_label();

                self.add_wip_block(wip, Jump(cond_entry));

                // Condition
                let WithStmts { stmts, val } = translator.convert_condition(true, condition, false)?;
                let mut cond_wip = self.new_wip_block(cond_entry);
                cond_wip.extend(stmts);
                self.add_wip_block(cond_wip, Branch(val, body_entry, next_entry));

                // Body
                self.break_labels.push(next_entry);
                self.continue_labels.push(cond_entry);

                let body_wip = self.new_wip_block(body_entry);
                let body_stuff = self.convert_stmt_help(translator, body_stmt, body_wip)?;
                if let Some(wip_body) = body_stuff {
                    self.add_wip_block(wip_body, Jump(cond_entry));
                }

                self.break_labels.pop();
                self.continue_labels.pop();

                //Return
                Ok(Some(self.new_wip_block(next_entry)))
            }

            CStmtKind::DoWhile { body: body_stmt, condition } => {
                let body_entry = self.fresh_label();
                let cond_entry = self.fresh_label();
                let next_entry = self.fresh_label();

                self.add_wip_block(wip, Jump(body_entry));

                // Body
                self.break_labels.push(next_entry);
                self.continue_labels.push(cond_entry);

                let body_wip = self.new_wip_block(body_entry);
                let body_stuff = self.convert_stmt_help(translator, body_stmt, body_wip)?;
                if let Some(wip_body) = body_stuff {
                    self.add_wip_block(wip_body, Jump(cond_entry));
                }

                self.break_labels.pop();
                self.continue_labels.pop();

                // Condition
                let WithStmts { stmts, val } = translator.convert_condition(true, condition, false)?;
                let mut cond_wip = self.new_wip_block(cond_entry);
                cond_wip.extend(stmts);
                self.add_wip_block(cond_wip, Branch(val, body_entry, next_entry));

                //Return
                Ok(Some(self.new_wip_block(next_entry)))
            }

            CStmtKind::ForLoop { init, condition, increment, body } => {
                let for_entry = self.fresh_label();
                let cond_entry = self.fresh_label();
                let body_entry = self.fresh_label();
                let incr_entry = self.fresh_label();
                let next_label = self.fresh_label();

                // Close off our WIP (it is important this happen _outside_ the `with_scope` call)
                self.add_wip_block(wip, Jump(for_entry));
                let wip = self.new_wip_block(for_entry);

                self.with_scope(translator, |slf| -> Result<(), String> {
                    // Init
                    let init_stuff = match init {
                        None => Some(wip),
                        Some(init) => slf.convert_stmt_help(translator, init, wip)?,
                    };
                    if let Some(wip_init) = init_stuff {
                        slf.add_wip_block(wip_init, Jump(cond_entry));
                    }

                    // Condition
                    if let Some(cond) = condition {
                        let WithStmts { stmts, val } = translator.convert_condition(true, cond, false)?;
                        let mut cond_wip = slf.new_wip_block(cond_entry);
                        cond_wip.extend(stmts);
                        slf.add_wip_block(cond_wip, Branch(val, body_entry, next_label));
                    } else {
                        slf.add_block(cond_entry, BasicBlock::new_jump(body_entry));
                    }

                    // Body
                    slf.break_labels.push(next_label);
                    slf.continue_labels.push(incr_entry);

                    let body_wip = slf.new_wip_block(body_entry);
                    let body_stuff = slf.convert_stmt_help(translator, body, body_wip)?;

                    if let Some(wip_body) = body_stuff {
                      slf.add_wip_block(wip_body, Jump(incr_entry));
                    }

                    slf.break_labels.pop();
                    slf.continue_labels.pop();

                    // Increment
                    match increment {
                        None => slf.add_block(incr_entry, BasicBlock::new_jump(cond_entry)),
                        Some(incr) => {
                          let incr_stmts = translator
                                  .convert_expr(ExprUse::Unused, incr, false)?
                                  .stmts;
                          let mut incr_wip = slf.new_wip_block(incr_entry);
                          incr_wip.extend(incr_stmts);
                          slf.add_wip_block(incr_wip, Jump(cond_entry));
                        }
                    }

                    Ok(())
                })?;
              
                // Return (it is important this happen _outside_ the `with_scope` call)
                Ok(Some(self.new_wip_block(next_label)))
            },

            CStmtKind::Label(sub_stmt) => {
                let this_label = Label::FromC(stmt_id);
                self.add_wip_block(wip, Jump(this_label));

                // Sub stmt
                let sub_stmt_wip = self.new_wip_block(this_label);
                self.convert_stmt_help(translator, sub_stmt, sub_stmt_wip)
            }

            CStmtKind::Goto(label_id) => {
                let tgt_label = Label::FromC(label_id);
                self.add_wip_block(wip, Jump(tgt_label));

                Ok(None)
            }

            CStmtKind::Compound(ref comp_stmts) => {

                // Close off our WIP (it is important this happen _outside_ the `with_scope` call)
                let compound_entry = self.fresh_label();
                self.add_wip_block(wip, Jump(compound_entry));
                let wip = self.new_wip_block(compound_entry);

                // We feed the optional output WIP into the WIP input of the next block
                let wip = self.with_scope(translator, |slf| -> Result<Option<WipBlock>, String> {
                    let mut wip = Some(wip);
                    for stmt in comp_stmts {
                        let new_label = slf.fresh_label();
                        let new_wip = wip.unwrap_or(slf.new_wip_block(new_label));
                        wip = slf.convert_stmt_help(translator, *stmt, new_wip)?;
                    }
                    Ok(wip)
                })?;

                // We need to close off the final WIP block (if there is even one) because whatever
                // follows will be in a different scope.
                Ok(wip.map(|last_wip| {
                    let new_label = self.fresh_label();
                    self.add_wip_block(last_wip, Jump(new_label));
                    self.new_wip_block(new_label)
                }))
            }

            CStmtKind::Expr(expr) => {
                wip.extend(translator.convert_expr(ExprUse::Unused, expr, false)?.stmts);

                // If we can tell the expression is going to diverge, there is no falling through to
                // the next block.
                let next = if translator.ast_context.expr_diverges(expr) {
                    self.add_wip_block(wip, End);
                    None
                } else {
                    Some(wip)
                };

                Ok(next)
            }

            CStmtKind::Break => {
                let tgt_label = *self.break_labels.last().expect("Nothing to 'break' to");
                self.add_wip_block(wip, Jump(tgt_label));

                Ok(None)
            }

            CStmtKind::Continue => {
                let tgt_label = *self.continue_labels.last().expect("Nothing to 'continue' to");
                self.add_wip_block(wip, Jump(tgt_label));

                Ok(None)
            }

            CStmtKind::Case(_case_expr, sub_stmt, cie) => {
                let this_label = Label::FromC(stmt_id);
                self.add_wip_block(wip, Jump(this_label));

                // Case
                let branch = match cie {
                    ConstIntExpr::U(n) =>
                        mk().lit_expr(mk().int_lit(n as u128, LitIntType::Unsuffixed)),

                    ConstIntExpr::I(n) if n >= 0 =>
                        mk().lit_expr(mk().int_lit(n as u128, LitIntType::Unsuffixed)),

                    ConstIntExpr::I(n) =>
                        mk().unary_expr(
                            syntax::ast::UnOp::Neg,
                            mk().lit_expr(mk().int_lit((-n) as u128, LitIntType::Unsuffixed))
                        ),
                };
                self.switch_expr_cases
                    .last_mut()
                    .expect("'case' outside of 'switch'")
                    .cases
                    .push((mk().lit_pat(branch), this_label));

                // Sub stmt
                let sub_stmt_wip = self.new_wip_block(this_label);
                self.convert_stmt_help(translator, sub_stmt, sub_stmt_wip)
            }

            CStmtKind::Default(sub_stmt) => {
                let this_label = Label::FromC(stmt_id);
                self.add_wip_block(wip, Jump(this_label));

                // Default case
                self.switch_expr_cases
                    .last_mut()
                    .expect("'default' outside of 'switch'")
                    .default
                    .get_or_insert(this_label);

                // Sub stmt
                let sub_stmt_wip = self.new_wip_block(this_label);
                self.convert_stmt_help(translator, sub_stmt, sub_stmt_wip)
            }

            CStmtKind::Switch { scrutinee, body: switch_body } => {
                let next_label = self.fresh_label();
                let body_label = self.fresh_label();

                // Convert the condition
                let WithStmts { stmts, val } = translator.convert_expr(ExprUse::RValue, scrutinee, false)?;
                wip.extend(stmts);

                let wip_label = wip.label;
                self.add_wip_block(wip, End); // NOTE: the `End` here is temporary and gets updated

                // Body
                self.break_labels.push(next_label);
                self.switch_expr_cases.push(SwitchCases::default());

                let body_wip = self.new_wip_block(body_label);
                let body_stuff = self.convert_stmt_help(translator, switch_body, body_wip)?;
                if let Some(body_wip) = body_stuff {
                    self.add_wip_block(body_wip, Jump(next_label));
                }

                self.break_labels.pop();
                let switch_case = self.switch_expr_cases.pop().expect("No 'SwitchCases' to pop");

                let mut cases: Vec<_> = switch_case.cases
                    .into_iter()
                    .map(|(p,lbl)| (vec![p],lbl))
                    .collect();
                cases.push((vec![mk().wild_pat()], switch_case.default.unwrap_or(next_label)));

                // Add the condition basic block terminator (we need the information built up during
                // the conversion of the body to make the right terminator)
                self.update_terminator(wip_label, Switch { expr: val, cases });

                // Return
                Ok(Some(self.new_wip_block(next_label)))
            }

            CStmtKind::Asm{is_volatile, ref asm, ref inputs, ref outputs, ref clobbers} => {
                wip.extend(translator.convert_asm(is_volatile, asm, inputs, outputs, clobbers)?);
                Ok(Some(wip))
            }
        }
    }
}


/// This impl block deals with pretty-printing control flow graphs into a format that `dot` can
/// consume. Compiling these files into images means running something like:
///
/// ```norun
/// dot -Tpng cfg_func.dot > cfg_func.png
/// ```
impl Cfg<Label,StmtOrDecl> {

    pub fn dump_dot_graph(
        &self,
        ctx: &TypedAstContext,
        store: &DeclStmtStore,
        file_path: String
    ) -> io::Result<()> {

        // Utility function for sanitizing strings
        fn sanitize_label(lbl: String) -> String {
            format!("{}\\l", lbl.replace("\t", "  ")
                                .replace("\\", "\\\\")
                                .replace("\"", "\\\"")
                                .replace("\n", "\\l"))
        }

        let mut file = File::create(file_path)?;
        file.write_all(b"digraph cfg {\n")?;
        file.write_all(b"  node [shape=box,fontname=Courier];\n")?;
        file.write_all(b"  edge [fontname=Courier,fontsize=10.0];\n")?;

        // Entry
        for (i, entry) in self.entries.iter().enumerate() {
            file.write_fmt(format_args!("  entry{} [shape=plaintext];\n", i))?;
            file.write_fmt(format_args!("  entry{} -> {};\n", i, entry.debug_print()))?;
        }

        // Rest of graph
        for (lbl, bb) in self.nodes.iter() {

            let pretty_terminator = match bb.terminator {
                End | Jump(_) => String::from(""),
                Branch(ref cond, _, _) => format!("\n{}",pprust::expr_to_string(cond.deref())),
                Switch { ref expr, .. } => format!("\n{}",pprust::expr_to_string(expr.deref())),
            };

            let defined = if bb.defined.is_empty() {
                format!("")
            } else {
                format!(
                    "\\ldefined: {{{}}}",
                    bb.defined
                        .iter()
                        .filter_map(|decl| ctx.index(*decl).kind.get_name())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            };

            let live = if bb.live.is_empty() {
                format!("")
            } else {
                format!(
                    "\\llive in: {{{}}}",
                    bb.live
                        .iter()
                        .filter_map(|decl| ctx.index(*decl).kind.get_name())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            };

            // A node
            file.write_fmt(format_args!(
                "  {} [label=\"{}:\\l-----{}{}\\l{}-----{}\"];\n",
                lbl.debug_print(),
                lbl.debug_print(),
                live,
                defined,
                format!("-----\\l{}", if bb.body.is_empty() {
                    String::from("")
                } else {
                    sanitize_label(bb.body
                        .iter()
                        .flat_map(|stmt_or_decl: &StmtOrDecl| -> Vec<Stmt> {
                            match stmt_or_decl {
                                &StmtOrDecl::Stmt(ref s) => vec![s.clone()],
                                &StmtOrDecl::Decl(ref d) => store.peek_decl_and_assign(*d).unwrap(),
                            }
                        })
                        .map(|stmt: Stmt| pprust::stmt_to_string(&stmt))
                        .collect::<Vec<String>>()
                        .join("\n")
                    )
                }),
                sanitize_label(pretty_terminator),
            ))?;

            // All the edges starting from this node
            let edges: Vec<(String, Label)> = match bb.terminator {
                End => vec![],
                Jump(tgt) => vec![(String::from(""),tgt)],
                Branch(_, tru, fal) => vec![
                    (String::from("true"),tru),
                    (String::from("false"),fal)
                ],
                Switch { ref cases, .. } => {
                    let mut cases: Vec<(String, Label)> = cases
                        .iter()
                        .map(|&(ref pats, tgt)| -> (String, Label) {
                            let pats: Vec<String> = pats
                                .iter()
                                .map(|p| pprust::pat_to_string(p.deref()))
                                .collect();

                            (pats.join(" | "), tgt)
                        })
                        .collect();
                    cases
                },
            };

            for (desc,tgt) in edges {
                file.write_fmt(format_args!(
                    "  {} -> {} [label=\"{}\"];\n",
                    lbl.debug_print(),
                    tgt.debug_print(),
                    sanitize_label(desc),
                ))?;
            }
        }

        file.write_all(b"}\n")?;

        Ok(())
    }
}
