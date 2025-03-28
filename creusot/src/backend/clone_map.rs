use std::cell::RefCell;

use crate::{
    backend::{Why3Generator, clone_map::elaborator::Expander, dependency::Dependency},
    contracts_items::{get_builtin, get_inv_function, is_bitwise},
    ctx::*,
    options::SpanMode,
    util::{erased_identity_for_item, path_of_span},
};
use elaborator::Strength;
use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools};
use once_map::unsync::OnceMap;
use petgraph::prelude::DiGraphMap;
use rustc_hir::{
    def::DefKind,
    def_id::{DefId, LocalDefId},
};
use rustc_macros::{TypeFoldable, TypeVisitable};
use rustc_middle::{
    mir::Promoted,
    ty::{self, GenericArgsRef, Ty, TyCtxt, TyKind, TypeFoldable, TypingEnv},
};
use rustc_span::{Span, Symbol};
use rustc_target::abi::{FieldIdx, VariantIdx};
use why3::{
    Ident, QName,
    declaration::{Attribute, Decl, Span as WSpan, TyDecl},
};

mod elaborator;

// Prelude modules
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, TypeVisitable, TypeFoldable)]
pub enum PreludeModule {
    Float32,
    Float64,
    Int,
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    UInt128,
    Char,
    Bool,
    MutBorrow,
    Slice,
    Opaque,
    Any,
}

pub(crate) trait Namer<'tcx> {
    fn item(&self, def_id: DefId, subst: GenericArgsRef<'tcx>) -> QName {
        let node = Dependency::Item(def_id, subst);
        self.dependency(node).qname()
    }

    fn ty(&self, def_id: DefId, subst: GenericArgsRef<'tcx>) -> QName {
        let ty = match self.tcx().def_kind(def_id) {
            DefKind::Enum | DefKind::Struct | DefKind::Union => {
                Ty::new_adt(self.tcx(), self.tcx().adt_def(def_id), subst)
            }
            DefKind::AssocTy => Ty::new_projection(self.tcx(), def_id, subst),
            DefKind::Closure => Ty::new_closure(self.tcx(), def_id, subst),
            DefKind::OpaqueTy => Ty::new_opaque(self.tcx(), def_id, subst),
            k => unreachable!("{k:?}"),
        };

        self.dependency(Dependency::Type(ty)).qname()
    }

    fn ty_param(&self, ty: Ty<'tcx>) -> QName {
        assert!(matches!(ty.kind(), TyKind::Param(_)));
        self.dependency(Dependency::Type(ty)).qname()
    }

    fn constructor(&self, def_id: DefId, subst: GenericArgsRef<'tcx>) -> QName {
        let node = Dependency::Item(def_id, subst);
        self.dependency(node).qname()
    }

    fn ty_inv(&self, ty: Ty<'tcx>) -> QName {
        let def_id = get_inv_function(self.tcx());
        let subst = self.tcx().mk_args(&[ty::GenericArg::from(ty)]);
        self.item(def_id, subst)
    }

    /// Creates a name for a struct or closure projection ie: x.field1
    ///
    /// * `def_id` - The id of the type or closure being projected
    /// * `subst` - Substitution that type is being accessed at
    /// * `ix` - The field in that constructor being accessed.
    fn field(&self, def_id: DefId, subst: GenericArgsRef<'tcx>, ix: FieldIdx) -> QName {
        let node = match self.tcx().def_kind(def_id) {
            DefKind::Closure => Dependency::ClosureAccessor(def_id, subst, ix.as_u32()),
            DefKind::Struct | DefKind::Union => {
                let field_did =
                    self.tcx().adt_def(def_id).variants()[VariantIdx::ZERO].fields[ix].did;
                Dependency::Item(field_did, subst)
            }
            _ => unreachable!(),
        };

        self.dependency(node).qname()
    }

    fn eliminator(&self, def_id: DefId, subst: GenericArgsRef<'tcx>) -> QName {
        self.dependency(Dependency::Eliminator(def_id, subst)).qname()
    }

    fn promoted(&self, def_id: LocalDefId, prom: Promoted) -> QName {
        self.dependency(Dependency::Promoted(def_id, prom)).qname()
    }

    fn normalize<T: TypeFoldable<TyCtxt<'tcx>>>(&self, ctx: &TranslationCtx<'tcx>, ty: T) -> T;

    fn import_prelude_module(&self, module: PreludeModule) {
        self.dependency(Dependency::Builtin(module));
    }

    fn prelude_module_name(&self, module: PreludeModule) -> Box<[Ident]> {
        self.dependency(Dependency::Builtin(module));
        let qname: QName = match (module, self.bitwise_mode()) {
            (PreludeModule::Float32, _) => "creusot.float.Float32.".into(),
            (PreludeModule::Float64, _) => "creusot.float.Float64.".into(),
            (PreludeModule::Int, _) => "mach.int.Int.".into(),
            (PreludeModule::Int8, false) => "creusot.int.Int8.".into(),
            (PreludeModule::Int16, false) => "creusot.int.Int16.".into(),
            (PreludeModule::Int32, false) => "creusot.int.Int32.".into(),
            (PreludeModule::Int64, false) => "creusot.int.Int64.".into(),
            (PreludeModule::Int128, false) => "creusot.int.Int128.".into(),
            (PreludeModule::UInt8, false) => "creusot.int.UInt8.".into(),
            (PreludeModule::UInt16, false) => "creusot.int.UInt16.".into(),
            (PreludeModule::UInt32, false) => "creusot.int.UInt32.".into(),
            (PreludeModule::UInt64, false) => "creusot.int.UInt64.".into(),
            (PreludeModule::UInt128, false) => "creusot.int.UInt128.".into(),
            (PreludeModule::Int8, true) => "creusot.int.Int8BW.".into(),
            (PreludeModule::Int16, true) => "creusot.int.Int16BW.".into(),
            (PreludeModule::Int32, true) => "creusot.int.Int32BW.".into(),
            (PreludeModule::Int64, true) => "creusot.int.Int64BW.".into(),
            (PreludeModule::Int128, true) => "creusot.int.Int128BW.".into(),
            (PreludeModule::UInt8, true) => "creusot.int.UInt8BW.".into(),
            (PreludeModule::UInt16, true) => "creusot.int.UInt16BW.".into(),
            (PreludeModule::UInt32, true) => "creusot.int.UInt32BW.".into(),
            (PreludeModule::UInt64, true) => "creusot.int.UInt64BW.".into(),
            (PreludeModule::UInt128, true) => "creusot.int.UInt128BW.".into(),
            (PreludeModule::Char, _) => "creusot.prelude.Char.".into(),
            (PreludeModule::Opaque, _) => "creusot.prelude.Opaque.".into(),
            (PreludeModule::Bool, _) => "creusot.prelude.Bool.".into(),
            (PreludeModule::MutBorrow, _) => "creusot.prelude.MutBorrow.".into(),
            (PreludeModule::Slice, _) => {
                format!("creusot.slice.Slice{}.", self.tcx().sess.target.pointer_width).into()
            }
            (PreludeModule::Any, _) => "creusot.prelude.Any.".into(),
        };
        qname.module
    }

    fn from_prelude(&self, module: PreludeModule, name: &str) -> QName {
        QName { module: self.prelude_module_name(module), name: name.into() }.without_search_path()
    }

    fn dependency(&self, dep: Dependency<'tcx>) -> &Kind;

    fn tcx(&self) -> TyCtxt<'tcx>;

    fn span(&self, span: Span) -> Option<Attribute>;

    fn bitwise_mode(&self) -> bool;
}

impl<'tcx> Namer<'tcx> for CloneNames<'tcx> {
    // TODO: get rid of this. It feels like it should be unnecessary
    fn normalize<T: TypeFoldable<TyCtxt<'tcx>>>(&self, _: &TranslationCtx<'tcx>, ty: T) -> T {
        self.tcx().normalize_erasing_regions(self.typing_env, ty)
    }

    fn dependency(&self, key: Dependency<'tcx>) -> &Kind {
        let key = key.erase_regions(self.tcx);
        self.insert(self.tcx, key)
    }

    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn span(&self, span: Span) -> Option<Attribute> {
        let path = path_of_span(self.tcx, span, &self.span_mode)?;
        let cnt = self.spans.len();
        let name = self.spans.insert(span, |_| {
            Box::new((
                Symbol::intern(&format!("s{}{cnt}", path.file_stem().unwrap().to_str().unwrap())),
                cnt,
            ))
        });
        Some(Attribute::NamedSpan(name.0.to_string()))
    }

    fn bitwise_mode(&self) -> bool {
        self.bitwise_mode
    }
}

impl<'tcx> CloneNames<'tcx> {
    fn insert(&self, tcx: TyCtxt<'tcx>, key: Dependency<'tcx>) -> &Kind {
        self.names.insert(key, |_| {
            if let Some((did, _)) = key.did()
                && let Some(why3_modl) = get_builtin(tcx, did)
            {
                let why3_modl =
                    why3_modl.as_str().replace("$BW$", if self.bitwise_mode { "BW" } else { "" });
                let qname = QName::from(why3_modl);
                if qname.module.is_empty() {
                    return Box::new(Kind::Named(Symbol::intern(&qname.name)));
                } else {
                    return Box::new(Kind::UsedBuiltin(qname));
                }
            }
            Box::new(
                key.base_ident(tcx).map_or(Kind::Unnamed, |base| {
                    Kind::Named(self.counts.borrow_mut().freshen(base))
                }),
            )
        })
    }

    fn bitwise_mode(&self) -> bool {
        self.bitwise_mode
    }
}

impl<'tcx> Namer<'tcx> for Dependencies<'tcx> {
    fn normalize<T: TypeFoldable<TyCtxt<'tcx>>>(&self, ctx: &TranslationCtx<'tcx>, ty: T) -> T {
        self.tcx().normalize_erasing_regions(ctx.typing_env(self.self_id), ty)
    }

    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn dependency(&self, key: Dependency<'tcx>) -> &Kind {
        let key = key.erase_regions(self.tcx);
        self.dep_set.borrow_mut().insert(key);
        self.names.dependency(key)
    }

    fn span(&self, span: Span) -> Option<Attribute> {
        self.names.span(span)
    }

    fn bitwise_mode(&self) -> bool {
        self.names.bitwise_mode()
    }
}

pub(crate) struct Dependencies<'tcx> {
    tcx: TyCtxt<'tcx>,
    names: CloneNames<'tcx>,

    // A hacky thing which is used to remember the dependncies we need to seed the expander with
    dep_set: RefCell<IndexSet<Dependency<'tcx>>>,

    pub(crate) self_id: DefId,
    pub(crate) self_subst: GenericArgsRef<'tcx>,
}

#[derive(Default, Clone)]
pub(crate) struct NameSupply {
    name_counts: IndexMap<Symbol, usize>,
}

pub(crate) struct CloneNames<'tcx> {
    tcx: TyCtxt<'tcx>,
    // To normalize during dependency stuff (deprecated)
    typing_env: TypingEnv<'tcx>,
    // Internal state, used to determine whether we should emit spans at all
    span_mode: SpanMode,
    // Should we use the BW version of the machine integer prelude?
    bitwise_mode: bool,

    /// Freshens a symbol by appending a number to the end
    counts: RefCell<NameSupply>,
    /// Tracks the name given to each dependency
    names: OnceMap<Dependency<'tcx>, Box<Kind>>,
    /// Maps spans to a unique name
    spans: OnceMap<Span, Box<(Symbol, usize)>>,
}

impl<'tcx> CloneNames<'tcx> {
    fn new(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        span_mode: SpanMode,
        bitwise_mode: bool,
    ) -> Self {
        CloneNames {
            tcx,
            typing_env,
            span_mode,
            bitwise_mode,
            counts: Default::default(),
            names: Default::default(),
            spans: Default::default(),
        }
    }
}

impl NameSupply {
    pub(crate) fn freshen(&mut self, sym: Symbol) -> Symbol {
        let count: usize = *self.name_counts.entry(sym).and_modify(|c| *c += 1).or_insert(0);
        // FIXME: if we don't do use the initial ident when count == 0, then the ident clashes
        // with local variables
        /*if count == 0 {
            sym
        } else {*/
        Symbol::intern(&format!("{sym}'{count}"))
        /*}*/
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// This does not corresponds to a defined symbol
    Unnamed,
    /// This symbol is locally defined
    Named(Symbol),
    /// Used, UsedBuiltin: the symbols in the last argument must be acompanied by a `use` statement in Why3
    UsedBuiltin(QName),
}

impl Kind {
    fn ident(&self) -> Ident {
        match self {
            Kind::Unnamed => panic!("Unnamed item"),
            Kind::Named(nm) => nm.as_str().into(),
            Kind::UsedBuiltin(_) => {
                panic!("cannot get ident of used module {self:?}")
            }
        }
    }

    fn qname(&self) -> QName {
        match self {
            Kind::Unnamed => panic!("Unnamed item"),
            Kind::Named(nm) => nm.as_str().into(),
            Kind::UsedBuiltin(qname) => qname.clone().without_search_path(),
        }
    }
}

impl<'tcx> Dependencies<'tcx> {
    pub(crate) fn new(ctx: &TranslationCtx<'tcx>, self_id: DefId) -> Self {
        let bw = is_bitwise(ctx.tcx, self_id);
        let names =
            CloneNames::new(ctx.tcx, ctx.typing_env(self_id), ctx.opts.span_mode.clone(), bw);
        debug!("cloning self: {:?}", self_id);
        let self_subst = erased_identity_for_item(ctx.tcx, self_id);
        let deps =
            Dependencies { tcx: ctx.tcx, self_id, self_subst, names, dep_set: Default::default() };

        let node = Dependency::Item(self_id, self_subst);
        deps.names.dependency(node);
        deps
    }

    pub(crate) fn provide_deps(mut self, ctx: &Why3Generator<'tcx>) -> Vec<Decl> {
        trace!("emitting dependencies for {:?}", self.self_id);
        let mut decls = Vec::new();

        let typing_env = ctx.typing_env(self.self_id);

        let self_node = Dependency::Item(self.self_id, self.self_subst);
        let graph = Expander::new(
            &mut self.names,
            self_node,
            typing_env,
            self.dep_set.into_inner().into_iter(),
        );

        // Update the clone graph with any new entries.
        let (graph, mut bodies) = graph.update_graph(ctx);

        for scc in petgraph::algo::tarjan_scc(&graph).into_iter() {
            if scc.iter().any(|node| node == &self_node) {
                assert_eq!(scc.len(), 1);
                bodies.remove(&scc[0]);
                continue;
            }

            // Then we construct a sub-graph ignoring weak edges.
            let mut subgraph = DiGraphMap::new();

            for n in &scc {
                subgraph.add_node(*n);
            }

            for n in &scc {
                for (_, t, str) in graph.edges_directed(*n, petgraph::Direction::Outgoing) {
                    if subgraph.contains_node(t) && *str == Strength::Strong {
                        subgraph.add_edge(*n, t, ());
                    }
                }
            }

            for scc in petgraph::algo::tarjan_scc(&subgraph).into_iter() {
                if scc.len() > 1
                    && scc.iter().any(|node| {
                        node.did().is_none_or(|(did, _)| {
                            !matches!(
                                self.tcx.def_kind(did),
                                DefKind::Struct
                                    | DefKind::Enum
                                    | DefKind::Union
                                    | DefKind::Variant
                                    | DefKind::Field
                            ) || get_builtin(self.tcx, did).is_some()
                        })
                    })
                {
                    ctx.crash_and_error(
                        ctx.def_span(scc[0].did().unwrap().0),
                        &format!("encountered a cycle during translation: {:?}", scc),
                    );
                }

                let mut bodies = scc
                    .iter()
                    .map(|node| bodies.remove(node).unwrap_or_else(|| panic!("not found {scc:?}")))
                    .collect::<Vec<_>>();

                if bodies.len() > 1 {
                    // Mutually recursive ADT
                    let tys = bodies
                        .into_iter()
                        .flatten()
                        .flat_map(|body| {
                            let Decl::TyDecl(TyDecl::Adt { tys }) = body else {
                                panic!("not an ADT decl")
                            };
                            tys
                        })
                        .collect();
                    decls.push(Decl::TyDecl(TyDecl::Adt { tys }))
                } else {
                    decls.extend(bodies.remove(0))
                }
            }
        }

        assert!(
            bodies.is_empty(),
            "unused bodies: {:?} for def {:?}",
            bodies.keys().collect::<Vec<_>>(),
            self.self_id
        );

        // Remove duplicates in `use` declarations, and move them at the beginning of the module
        let (uses, mut decls): (IndexSet<_>, Vec<_>) = decls
            .into_iter()
            .flat_map(|d| {
                if let Decl::UseDecls(u) = d { Either::Left(u) } else { Either::Right([d]) }
                    .factor_into_iter()
            })
            .partition_map(|x| x);
        if !uses.is_empty() {
            decls.insert(0, Decl::UseDecls(uses.into_iter().collect()));
        }

        let spans: Box<[WSpan]> = self
            .names
            .spans
            .into_iter()
            .sorted_by_key(|(_, b)| b.1)
            .filter_map(|(sp, b)| {
                let (path, start_line, start_column, end_line, end_column) =
                    if let Some(Attribute::Span(path, l1, c1, l2, c2)) = ctx.span_attr(sp) {
                        (path, l1, c1, l2, c2)
                    } else {
                        return None;
                    };
                let name = b.0.as_str().into();
                Some(WSpan { name, path, start_line, start_column, end_line, end_column })
            })
            .collect();

        let dependencies = if spans.is_empty() {
            decls
        } else {
            let mut tmp = vec![Decl::LetSpans(spans)];
            tmp.extend(decls);
            tmp
        };

        dependencies
    }
}
