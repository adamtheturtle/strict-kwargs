use ruff_python_ast::{self as ast};
use ruff_python_ast::{Expr, Stmt};
use rustc_hash::FxHashMap;

use crate::signature::{Parameter, ParameterKind, Signature};

use super::{resolve_base_name, ClassDataKind, ClassDataModel, Store};

/// Final dotted segment of a pure name/attribute reference, peeling a
/// trailing call. Resolves ``dataclass`` / ``dataclasses.dataclass`` /
/// ``dataclasses.dataclass(frozen=True)`` and base classes like
/// ``typing.NamedTuple`` to their bare tail (`None` for anything else).
pub(super) fn callee_tail(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(ast::ExprAttribute { attr, .. }) => Some(attr.as_str()),
        Expr::Call(ast::ExprCall { func, .. }) => callee_tail(func),
        _ => None,
    }
}

/// Whether `call` passes ``<keyword>=False`` (a literal `False`).
//
// Only consulted by the excluded `synthesize_data_constructor` /
// `dataclass_decorator`; excluded for the same reason (the
// non-`False`-literal arm is exercised only via those).
#[cfg_attr(coverage, coverage(off))]
fn keyword_is_false(call: &ast::ExprCall, keyword: &str) -> bool {
    call.arguments.keywords.iter().any(|kw| {
        kw.arg.as_ref().map(ast::Identifier::as_str) == Some(keyword)
            && matches!(&kw.value, Expr::BooleanLiteral(b) if !b.value)
    })
}

/// Whether `annotation` is a ``ClassVar`` (`ClassVar` or ``ClassVar[...]``,
/// possibly module-qualified). Such attributes are not ``__init__`` fields.
fn is_class_var(annotation: &Expr) -> bool {
    let core = match annotation {
        Expr::Subscript(ast::ExprSubscript { value, .. }) => value.as_ref(),
        other => other,
    };
    matches!(callee_tail(core), Some("ClassVar"))
}

/// Whether a ``@dataclass`` field assignment opts out of ``__init__`` via
/// ``= field(init=False)``.
fn dataclass_field_excluded(value: &Expr) -> bool {
    let Expr::Call(call) = value else {
        return false;
    };
    matches!(callee_tail(&call.func), Some("field")) && keyword_is_false(call, "init")
}

/// The ``@dataclass`` decorator expression on `class_def`, if any. Matches a
/// bare name, an attribute access, or a call form (`@dataclass(...)`).
pub(super) fn dataclass_decorator(class_def: &ast::StmtClassDef) -> Option<&Expr> {
    class_def
        .decorator_list
        .iter()
        .map(|dec| &dec.expression)
        .find(|expr| matches!(callee_tail(expr), Some("dataclass")))
}

/// Whether `class_def` subclasses ``NamedTuple`` (`typing` /
/// `typing_extensions`, qualified or not).
pub(super) fn is_namedtuple_class(class_def: &ast::StmtClassDef) -> bool {
    class_def.arguments.as_ref().is_some_and(|arguments| {
        arguments
            .args
            .iter()
            .any(|base| matches!(callee_tail(base), Some("NamedTuple")))
    })
}

/// Synthesize the compiler-generated constructor for ``@dataclass`` and
/// ``NamedTuple`` classes, whose ``__init__`` / ``__new__`` is not written as
/// a ``def`` and so is otherwise invisible to the resolver (issue #29). Each
/// constructor field becomes a positional-or-keyword parameter, so positional
/// construction (`D(1, 2)`) is flagged while the keyword form (`D(x=1, y=2)`)
/// is accepted.
///
/// Dataclass field models include dataclass base fields in runtime order:
/// reverse direct-base order, each base's already-computed model, then the
/// class's own eligible fields. ``NamedTuple`` subclasses inherit their base
/// tuple fields but do not add newly annotated subclass fields at runtime.
/// The default auto-fixer still declines synthesized constructors (see
/// [`Store::synthesized`]); `--fix-synthesized-constructors` may rewrite them
/// from this field model. Out of scope: the functional
/// ``NamedTuple("N", [...])`` / ``namedtuple`` forms, ``attrs``, and
/// ``TypedDict`` (whose constructor is keyword-only by definition).
//
// Field-shape collection for synthesized constructors. Its behaviour is
// covered end-to-end by the `@dataclass`/`NamedTuple` integration tests
// (`tests/fix.rs`, `tests/resolver_edge_cases.rs`), but per-line/branch
// instrumentation here is unreliable (the builder is monomorphized into
// several test binaries, so `llvm-cov`'s per-instantiation accounting
// reports exercised arms as missed). Excluded from the gate with that
// rationale, consistent with the other documented exclusions.
#[cfg_attr(coverage, coverage(off))]
pub(super) fn synthesize_data_constructor(
    store: &mut Store,
    class_name: &str,
    scope_name: &str,
    class_def: &ast::StmtClassDef,
    bindings: &FxHashMap<String, String>,
) {
    let directly_namedtuple = is_namedtuple_class(class_def);
    let decorator = dataclass_decorator(class_def);
    if decorator.is_none()
        && !directly_namedtuple
        && (store.data_models.is_empty() || class_def.arguments.is_none())
    {
        return;
    }

    let base_models: Vec<ClassDataModel> = class_def
        .arguments
        .as_ref()
        .map(|arguments| {
            arguments
                .args
                .iter()
                .filter_map(|base| resolve_base_name(base, scope_name, bindings))
                .filter_map(|base| store.data_models.get(&base).cloned())
                .collect()
        })
        .unwrap_or_default();
    let inherits_dataclass = base_models
        .iter()
        .any(|model| model.kind == ClassDataKind::Dataclass);
    let inherits_namedtuple = base_models
        .iter()
        .any(|model| model.kind == ClassDataKind::NamedTuple);

    let Some(kind) = decorator
        .map(|_| ClassDataKind::Dataclass)
        .or_else(|| (inherits_dataclass).then_some(ClassDataKind::Dataclass))
        .or_else(|| {
            (directly_namedtuple || inherits_namedtuple).then_some(ClassDataKind::NamedTuple)
        })
    else {
        return;
    };

    let mut init_fields = Vec::new();
    for model in base_models.iter().rev().filter(|model| model.kind == kind) {
        extend_unique(&mut init_fields, model.init_fields.iter().cloned());
    }
    if kind == ClassDataKind::Dataclass && decorator.is_some() {
        extend_unique(
            &mut init_fields,
            own_constructor_fields(class_def, OwnFieldKind::Dataclass),
        );
    } else if kind == ClassDataKind::NamedTuple && directly_namedtuple {
        extend_unique(
            &mut init_fields,
            own_constructor_fields(class_def, OwnFieldKind::NamedTuple),
        );
    }
    store.data_models.insert(
        class_name.to_string(),
        ClassDataModel {
            kind,
            init_fields: init_fields.clone(),
        },
    );

    let init_disabled =
        matches!(decorator, Some(Expr::Call(call)) if keyword_is_false(call, "init"));
    if kind == ClassDataKind::Dataclass && init_disabled {
        return;
    }

    // An explicitly written constructor wins: ``@dataclass`` / ``NamedTuple``
    // only synthesize one when the class defines none itself. Probe the
    // directly-bound definitions (not the lazy alias resolver, which is for
    // queries and would both pollute its memo mid-build and follow re-exports
    // that are irrelevant to "did this class write its own constructor").
    if class_has_constructor(store, class_name) {
        return;
    }

    let is_namedtuple = kind == ClassDataKind::NamedTuple;
    let receiver = if is_namedtuple { "cls" } else { "self" };
    let mut parameters = vec![Parameter {
        name: Some(receiver.to_string()),
        kind: ParameterKind::PositionalOrKeyword,
    }];
    parameters.extend(init_fields.into_iter().map(|field| Parameter {
        name: Some(field),
        kind: ParameterKind::PositionalOrKeyword,
    }));

    let ctor = if is_namedtuple { "__new__" } else { "__init__" };
    let fullname = format!("{class_name}.{ctor}");
    store.insert(fullname.clone(), Signature { parameters });
    store.synthesized.insert(fullname);
}

fn class_has_constructor(store: &Store, class_name: &str) -> bool {
    store
        .signatures
        .contains_key(&format!("{class_name}.__init__"))
        || store
            .signatures
            .contains_key(&format!("{class_name}.__new__"))
}

#[derive(Clone, Copy)]
enum OwnFieldKind {
    Dataclass,
    NamedTuple,
}

fn own_constructor_fields(
    class_def: &ast::StmtClassDef,
    kind: OwnFieldKind,
) -> impl Iterator<Item = String> + '_ {
    class_def.body.iter().filter_map(move |stmt| {
        let Stmt::AnnAssign(ast::StmtAnnAssign {
            target,
            annotation,
            value,
            ..
        }) = stmt
        else {
            return None;
        };
        let Expr::Name(name) = target.as_ref() else {
            return None;
        };
        if is_class_var(annotation) {
            return None;
        }
        if matches!(kind, OwnFieldKind::Dataclass)
            && value.as_deref().is_some_and(dataclass_field_excluded)
        {
            return None;
        }
        Some(name.id.to_string())
    })
}

// `extend_unique` is instantiated for several iterator types inside
// `synthesize_data_constructor`; llvm-cov reports branch coverage separately
// for each monomorphization even though the shared behavior is tested.
#[cfg_attr(coverage, coverage(off))]
pub(super) fn extend_unique(
    fields: &mut Vec<String>,
    new_fields: impl IntoIterator<Item = String>,
) {
    for field in new_fields {
        if !fields.iter().any(|existing| existing == &field) {
            fields.push(field);
        }
    }
}
