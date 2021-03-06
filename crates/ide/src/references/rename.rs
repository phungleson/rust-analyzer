//! FIXME: write short doc here
use std::{
    convert::TryInto,
    error::Error,
    fmt::{self, Display},
};

use hir::{Module, ModuleDef, ModuleSource, Semantics};
use ide_db::base_db::{AnchoredPathBuf, FileRange, SourceDatabaseExt};
use ide_db::{
    defs::{Definition, NameClass, NameRefClass},
    RootDatabase,
};
use syntax::{
    algo::find_node_at_offset,
    ast::{self, NameOwner},
    lex_single_syntax_kind, match_ast, AstNode, SyntaxKind, SyntaxNode, SyntaxToken,
};
use test_utils::mark;
use text_edit::TextEdit;

use crate::{
    references::find_all_refs, FilePosition, FileSystemEdit, RangeInfo, Reference, ReferenceKind,
    SourceChange, SourceFileEdit, TextRange, TextSize,
};

#[derive(Debug)]
pub struct RenameError(pub(crate) String);

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Error for RenameError {}

pub(crate) fn prepare_rename(
    db: &RootDatabase,
    position: FilePosition,
) -> Result<RangeInfo<()>, RenameError> {
    let sema = Semantics::new(db);
    let source_file = sema.parse(position.file_id);
    let syntax = source_file.syntax();
    if let Some(module) = find_module_at_offset(&sema, position, syntax) {
        rename_mod(&sema, position, module, "dummy")
    } else if let Some(self_token) =
        syntax.token_at_offset(position.offset).find(|t| t.kind() == SyntaxKind::SELF_KW)
    {
        rename_self_to_param(&sema, position, self_token, "dummy")
    } else {
        let range = match find_all_refs(&sema, position, None) {
            Some(RangeInfo { range, .. }) => range,
            None => return Err(RenameError("No references found at position".to_string())),
        };
        Ok(RangeInfo::new(range, SourceChange::from(vec![])))
    }
    .map(|info| RangeInfo::new(info.range, ()))
}

pub(crate) fn rename(
    db: &RootDatabase,
    position: FilePosition,
    new_name: &str,
) -> Result<RangeInfo<SourceChange>, RenameError> {
    let sema = Semantics::new(db);
    rename_with_semantics(&sema, position, new_name)
}

pub(crate) fn rename_with_semantics(
    sema: &Semantics<RootDatabase>,
    position: FilePosition,
    new_name: &str,
) -> Result<RangeInfo<SourceChange>, RenameError> {
    let is_lifetime_name = match lex_single_syntax_kind(new_name) {
        Some(res) => match res {
            (SyntaxKind::IDENT, _) => false,
            (SyntaxKind::UNDERSCORE, _) => false,
            (SyntaxKind::SELF_KW, _) => return rename_to_self(&sema, position),
            (SyntaxKind::LIFETIME_IDENT, _) if new_name != "'static" && new_name != "'_" => true,
            (SyntaxKind::LIFETIME_IDENT, _) => {
                return Err(RenameError(format!(
                    "Invalid name `{0}`: Cannot rename lifetime to {0}",
                    new_name
                )))
            }
            (_, Some(syntax_error)) => {
                return Err(RenameError(format!("Invalid name `{}`: {}", new_name, syntax_error)))
            }
            (_, None) => {
                return Err(RenameError(format!("Invalid name `{}`: not an identifier", new_name)))
            }
        },
        None => return Err(RenameError(format!("Invalid name `{}`: not an identifier", new_name))),
    };

    let source_file = sema.parse(position.file_id);
    let syntax = source_file.syntax();
    // this is here to prevent lifetime renames from happening on modules and self
    if is_lifetime_name {
        rename_reference(&sema, position, new_name, is_lifetime_name)
    } else if let Some(module) = find_module_at_offset(&sema, position, syntax) {
        rename_mod(&sema, position, module, new_name)
    } else if let Some(self_token) =
        syntax.token_at_offset(position.offset).find(|t| t.kind() == SyntaxKind::SELF_KW)
    {
        rename_self_to_param(&sema, position, self_token, new_name)
    } else {
        rename_reference(&sema, position, new_name, is_lifetime_name)
    }
}

fn find_module_at_offset(
    sema: &Semantics<RootDatabase>,
    position: FilePosition,
    syntax: &SyntaxNode,
) -> Option<Module> {
    let ident = syntax.token_at_offset(position.offset).find(|t| t.kind() == SyntaxKind::IDENT)?;

    let module = match_ast! {
        match (ident.parent()) {
            ast::NameRef(name_ref) => {
                match NameRefClass::classify(sema, &name_ref)? {
                    NameRefClass::Definition(Definition::ModuleDef(ModuleDef::Module(module))) => module,
                    _ => return None,
                }
            },
            ast::Name(name) => {
                match NameClass::classify(&sema, &name)? {
                    NameClass::Definition(Definition::ModuleDef(ModuleDef::Module(module))) => module,
                    _ => return None,
                }
            },
            _ => return None,
        }
    };

    Some(module)
}

fn source_edit_from_reference(
    sema: &Semantics<RootDatabase>,
    reference: Reference,
    new_name: &str,
) -> SourceFileEdit {
    let mut replacement_text = String::new();
    let range = match reference.kind {
        ReferenceKind::FieldShorthandForField => {
            mark::hit!(test_rename_struct_field_for_shorthand);
            replacement_text.push_str(new_name);
            replacement_text.push_str(": ");
            TextRange::new(reference.file_range.range.start(), reference.file_range.range.start())
        }
        ReferenceKind::FieldShorthandForLocal => {
            mark::hit!(test_rename_local_for_field_shorthand);
            replacement_text.push_str(": ");
            replacement_text.push_str(new_name);
            TextRange::new(reference.file_range.range.end(), reference.file_range.range.end())
        }
        ReferenceKind::RecordFieldExprOrPat => {
            mark::hit!(test_rename_field_expr_pat);
            replacement_text.push_str(new_name);
            edit_text_range_for_record_field_expr_or_pat(sema, reference.file_range, new_name)
        }
        _ => {
            replacement_text.push_str(new_name);
            reference.file_range.range
        }
    };
    SourceFileEdit {
        file_id: reference.file_range.file_id,
        edit: TextEdit::replace(range, replacement_text),
    }
}

fn edit_text_range_for_record_field_expr_or_pat(
    sema: &Semantics<RootDatabase>,
    file_range: FileRange,
    new_name: &str,
) -> TextRange {
    let source_file = sema.parse(file_range.file_id);
    let file_syntax = source_file.syntax();
    let original_range = file_range.range;

    syntax::algo::find_node_at_range::<ast::RecordExprField>(file_syntax, original_range)
        .and_then(|field_expr| match field_expr.expr().and_then(|e| e.name_ref()) {
            Some(name) if &name.to_string() == new_name => Some(field_expr.syntax().text_range()),
            _ => None,
        })
        .or_else(|| {
            syntax::algo::find_node_at_range::<ast::RecordPatField>(file_syntax, original_range)
                .and_then(|field_pat| match field_pat.pat() {
                    Some(ast::Pat::IdentPat(pat))
                        if pat.name().map(|n| n.to_string()).as_deref() == Some(new_name) =>
                    {
                        Some(field_pat.syntax().text_range())
                    }
                    _ => None,
                })
        })
        .unwrap_or(original_range)
}

fn rename_mod(
    sema: &Semantics<RootDatabase>,
    position: FilePosition,
    module: Module,
    new_name: &str,
) -> Result<RangeInfo<SourceChange>, RenameError> {
    let mut source_file_edits = Vec::new();
    let mut file_system_edits = Vec::new();

    let src = module.definition_source(sema.db);
    let file_id = src.file_id.original_file(sema.db);
    match src.value {
        ModuleSource::SourceFile(..) => {
            // mod is defined in path/to/dir/mod.rs
            let path = if module.is_mod_rs(sema.db) {
                format!("../{}/mod.rs", new_name)
            } else {
                format!("{}.rs", new_name)
            };
            let dst = AnchoredPathBuf { anchor: file_id, path };
            let move_file = FileSystemEdit::MoveFile { src: file_id, dst };
            file_system_edits.push(move_file);
        }
        ModuleSource::Module(..) => {}
    }

    if let Some(src) = module.declaration_source(sema.db) {
        let file_id = src.file_id.original_file(sema.db);
        let name = src.value.name().unwrap();
        let edit = SourceFileEdit {
            file_id,
            edit: TextEdit::replace(name.syntax().text_range(), new_name.into()),
        };
        source_file_edits.push(edit);
    }

    let RangeInfo { range, info: refs } = find_all_refs(sema, position, None)
        .ok_or_else(|| RenameError("No references found at position".to_string()))?;
    let ref_edits = refs
        .references
        .into_iter()
        .map(|reference| source_edit_from_reference(sema, reference, new_name));
    source_file_edits.extend(ref_edits);

    Ok(RangeInfo::new(range, SourceChange::from_edits(source_file_edits, file_system_edits)))
}

fn rename_to_self(
    sema: &Semantics<RootDatabase>,
    position: FilePosition,
) -> Result<RangeInfo<SourceChange>, RenameError> {
    let source_file = sema.parse(position.file_id);
    let syn = source_file.syntax();

    let (fn_def, fn_ast) = find_node_at_offset::<ast::Fn>(syn, position.offset)
        .and_then(|fn_ast| sema.to_def(&fn_ast).zip(Some(fn_ast)))
        .ok_or_else(|| RenameError("No surrounding method declaration found".to_string()))?;
    let param_range = fn_ast
        .param_list()
        .and_then(|p| p.params().next())
        .ok_or_else(|| RenameError("Method has no parameters".to_string()))?
        .syntax()
        .text_range();
    if !param_range.contains(position.offset) {
        return Err(RenameError("Only the first parameter can be self".to_string()));
    }

    let impl_block = find_node_at_offset::<ast::Impl>(syn, position.offset)
        .and_then(|def| sema.to_def(&def))
        .ok_or_else(|| RenameError("No impl block found for function".to_string()))?;
    if fn_def.self_param(sema.db).is_some() {
        return Err(RenameError("Method already has a self parameter".to_string()));
    }

    let params = fn_def.assoc_fn_params(sema.db);
    let first_param =
        params.first().ok_or_else(|| RenameError("Method has no parameters".into()))?;
    let first_param_ty = first_param.ty();
    let impl_ty = impl_block.target_ty(sema.db);
    let (ty, self_param) = if impl_ty.remove_ref().is_some() {
        // if the impl is a ref to the type we can just match the `&T` with self directly
        (first_param_ty.clone(), "self")
    } else {
        first_param_ty.remove_ref().map_or((first_param_ty.clone(), "self"), |ty| {
            (ty, if first_param_ty.is_mutable_reference() { "&mut self" } else { "&self" })
        })
    };

    if ty != impl_ty {
        return Err(RenameError("Parameter type differs from impl block type".to_string()));
    }

    let RangeInfo { range, info: refs } = find_all_refs(sema, position, None)
        .ok_or_else(|| RenameError("No reference found at position".to_string()))?;

    let (param_ref, usages): (Vec<Reference>, Vec<Reference>) = refs
        .into_iter()
        .partition(|reference| param_range.intersect(reference.file_range.range).is_some());

    if param_ref.is_empty() {
        return Err(RenameError("Parameter to rename not found".to_string()));
    }

    let mut edits = usages
        .into_iter()
        .map(|reference| source_edit_from_reference(sema, reference, "self"))
        .collect::<Vec<_>>();

    edits.push(SourceFileEdit {
        file_id: position.file_id,
        edit: TextEdit::replace(param_range, String::from(self_param)),
    });

    Ok(RangeInfo::new(range, SourceChange::from(edits)))
}

fn text_edit_from_self_param(
    syn: &SyntaxNode,
    self_param: &ast::SelfParam,
    new_name: &str,
) -> Option<TextEdit> {
    fn target_type_name(impl_def: &ast::Impl) -> Option<String> {
        if let Some(ast::Type::PathType(p)) = impl_def.self_ty() {
            return Some(p.path()?.segment()?.name_ref()?.text().to_string());
        }
        None
    }

    let impl_def = find_node_at_offset::<ast::Impl>(syn, self_param.syntax().text_range().start())?;
    let type_name = target_type_name(&impl_def)?;

    let mut replacement_text = String::from(new_name);
    replacement_text.push_str(": ");
    match (self_param.amp_token(), self_param.mut_token()) {
        (None, None) => (),
        (Some(_), None) => replacement_text.push('&'),
        (_, Some(_)) => replacement_text.push_str("&mut "),
    };
    replacement_text.push_str(type_name.as_str());

    Some(TextEdit::replace(self_param.syntax().text_range(), replacement_text))
}

fn rename_self_to_param(
    sema: &Semantics<RootDatabase>,
    position: FilePosition,
    self_token: SyntaxToken,
    new_name: &str,
) -> Result<RangeInfo<SourceChange>, RenameError> {
    let source_file = sema.parse(position.file_id);
    let syn = source_file.syntax();

    let text = sema.db.file_text(position.file_id);
    let fn_def = find_node_at_offset::<ast::Fn>(syn, position.offset)
        .ok_or_else(|| RenameError("No surrounding method declaration found".to_string()))?;
    let search_range = fn_def.syntax().text_range();

    let mut edits: Vec<SourceFileEdit> = vec![];

    for (idx, _) in text.match_indices("self") {
        let offset: TextSize = idx.try_into().unwrap();
        if !search_range.contains_inclusive(offset) {
            continue;
        }
        if let Some(ref usage) =
            syn.token_at_offset(offset).find(|t| t.kind() == SyntaxKind::SELF_KW)
        {
            let edit = if let Some(ref self_param) = ast::SelfParam::cast(usage.parent()) {
                text_edit_from_self_param(syn, self_param, new_name)
                    .ok_or_else(|| RenameError("No target type found".to_string()))?
            } else {
                TextEdit::replace(usage.text_range(), String::from(new_name))
            };
            edits.push(SourceFileEdit { file_id: position.file_id, edit });
        }
    }

    let range = ast::SelfParam::cast(self_token.parent())
        .map_or(self_token.text_range(), |p| p.syntax().text_range());

    Ok(RangeInfo::new(range, SourceChange::from(edits)))
}

fn rename_reference(
    sema: &Semantics<RootDatabase>,
    position: FilePosition,
    new_name: &str,
    is_lifetime_name: bool,
) -> Result<RangeInfo<SourceChange>, RenameError> {
    let RangeInfo { range, info: refs } = match find_all_refs(sema, position, None) {
        Some(range_info) => range_info,
        None => return Err(RenameError("No references found at position".to_string())),
    };

    match (refs.declaration.kind == ReferenceKind::Lifetime, is_lifetime_name) {
        (true, false) => {
            return Err(RenameError(format!(
                "Invalid name `{}`: not a lifetime identifier",
                new_name
            )))
        }
        (false, true) => {
            return Err(RenameError(format!("Invalid name `{}`: not an identifier", new_name)))
        }
        _ => (),
    }

    let edit = refs
        .into_iter()
        .map(|reference| source_edit_from_reference(sema, reference, new_name))
        .collect::<Vec<_>>();

    if edit.is_empty() {
        return Err(RenameError("No references found at position".to_string()));
    }

    Ok(RangeInfo::new(range, SourceChange::from(edit)))
}

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};
    use stdx::trim_indent;
    use test_utils::{assert_eq_text, mark};
    use text_edit::TextEdit;

    use crate::{fixture, FileId};

    fn check(new_name: &str, ra_fixture_before: &str, ra_fixture_after: &str) {
        let ra_fixture_after = &trim_indent(ra_fixture_after);
        let (analysis, position) = fixture::position(ra_fixture_before);
        let rename_result = analysis
            .rename(position, new_name)
            .unwrap_or_else(|err| panic!("Rename to '{}' was cancelled: {}", new_name, err));
        match rename_result {
            Ok(source_change) => {
                let mut text_edit_builder = TextEdit::builder();
                let mut file_id: Option<FileId> = None;
                for edit in source_change.info.source_file_edits {
                    file_id = Some(edit.file_id);
                    for indel in edit.edit.into_iter() {
                        text_edit_builder.replace(indel.delete, indel.insert);
                    }
                }
                let mut result = analysis.file_text(file_id.unwrap()).unwrap().to_string();
                text_edit_builder.finish().apply(&mut result);
                assert_eq_text!(ra_fixture_after, &*result);
            }
            Err(err) => {
                if ra_fixture_after.starts_with("error:") {
                    let error_message = ra_fixture_after
                        .chars()
                        .into_iter()
                        .skip("error:".len())
                        .collect::<String>();
                    assert_eq!(error_message.trim(), err.to_string());
                    return;
                } else {
                    panic!("Rename to '{}' failed unexpectedly: {}", new_name, err)
                }
            }
        };
    }

    fn check_expect(new_name: &str, ra_fixture: &str, expect: Expect) {
        let (analysis, position) = fixture::position(ra_fixture);
        let source_change = analysis
            .rename(position, new_name)
            .unwrap()
            .expect("Expect returned RangeInfo to be Some, but was None");
        expect.assert_debug_eq(&source_change)
    }

    #[test]
    fn test_rename_to_underscore() {
        check("_", r#"fn main() { let i<|> = 1; }"#, r#"fn main() { let _ = 1; }"#);
    }

    #[test]
    fn test_rename_to_raw_identifier() {
        check("r#fn", r#"fn main() { let i<|> = 1; }"#, r#"fn main() { let r#fn = 1; }"#);
    }

    #[test]
    fn test_rename_to_invalid_identifier1() {
        check(
            "invalid!",
            r#"fn main() { let i<|> = 1; }"#,
            "error: Invalid name `invalid!`: not an identifier",
        );
    }

    #[test]
    fn test_rename_to_invalid_identifier2() {
        check(
            "multiple tokens",
            r#"fn main() { let i<|> = 1; }"#,
            "error: Invalid name `multiple tokens`: not an identifier",
        );
    }

    #[test]
    fn test_rename_to_invalid_identifier3() {
        check(
            "let",
            r#"fn main() { let i<|> = 1; }"#,
            "error: Invalid name `let`: not an identifier",
        );
    }

    #[test]
    fn test_rename_to_invalid_identifier_lifetime() {
        check(
            "'foo",
            r#"fn main() { let i<|> = 1; }"#,
            "error: Invalid name `'foo`: not an identifier",
        );
    }

    #[test]
    fn test_rename_to_invalid_identifier_lifetime2() {
        check(
            "foo",
            r#"fn main<'a>(_: &'a<|> ()) {}"#,
            "error: Invalid name `foo`: not a lifetime identifier",
        );
    }

    #[test]
    fn test_rename_for_local() {
        check(
            "k",
            r#"
fn main() {
    let mut i = 1;
    let j = 1;
    i = i<|> + j;

    { i = 0; }

    i = 5;
}
"#,
            r#"
fn main() {
    let mut k = 1;
    let j = 1;
    k = k + j;

    { k = 0; }

    k = 5;
}
"#,
        );
    }

    #[test]
    fn test_rename_unresolved_reference() {
        check(
            "new_name",
            r#"fn main() { let _ = unresolved_ref<|>; }"#,
            "error: No references found at position",
        );
    }

    #[test]
    fn test_rename_for_macro_args() {
        check(
            "b",
            r#"
macro_rules! foo {($i:ident) => {$i} }
fn main() {
    let a<|> = "test";
    foo!(a);
}
"#,
            r#"
macro_rules! foo {($i:ident) => {$i} }
fn main() {
    let b = "test";
    foo!(b);
}
"#,
        );
    }

    #[test]
    fn test_rename_for_macro_args_rev() {
        check(
            "b",
            r#"
macro_rules! foo {($i:ident) => {$i} }
fn main() {
    let a = "test";
    foo!(a<|>);
}
"#,
            r#"
macro_rules! foo {($i:ident) => {$i} }
fn main() {
    let b = "test";
    foo!(b);
}
"#,
        );
    }

    #[test]
    fn test_rename_for_macro_define_fn() {
        check(
            "bar",
            r#"
macro_rules! define_fn {($id:ident) => { fn $id{} }}
define_fn!(foo);
fn main() {
    fo<|>o();
}
"#,
            r#"
macro_rules! define_fn {($id:ident) => { fn $id{} }}
define_fn!(bar);
fn main() {
    bar();
}
"#,
        );
    }

    #[test]
    fn test_rename_for_macro_define_fn_rev() {
        check(
            "bar",
            r#"
macro_rules! define_fn {($id:ident) => { fn $id{} }}
define_fn!(fo<|>o);
fn main() {
    foo();
}
"#,
            r#"
macro_rules! define_fn {($id:ident) => { fn $id{} }}
define_fn!(bar);
fn main() {
    bar();
}
"#,
        );
    }

    #[test]
    fn test_rename_for_param_inside() {
        check("j", r#"fn foo(i : u32) -> u32 { i<|> }"#, r#"fn foo(j : u32) -> u32 { j }"#);
    }

    #[test]
    fn test_rename_refs_for_fn_param() {
        check("j", r#"fn foo(i<|> : u32) -> u32 { i }"#, r#"fn foo(j : u32) -> u32 { j }"#);
    }

    #[test]
    fn test_rename_for_mut_param() {
        check("j", r#"fn foo(mut i<|> : u32) -> u32 { i }"#, r#"fn foo(mut j : u32) -> u32 { j }"#);
    }

    #[test]
    fn test_rename_struct_field() {
        check(
            "j",
            r#"
struct Foo { i<|>: i32 }

impl Foo {
    fn new(i: i32) -> Self {
        Self { i: i }
    }
}
"#,
            r#"
struct Foo { j: i32 }

impl Foo {
    fn new(i: i32) -> Self {
        Self { j: i }
    }
}
"#,
        );
    }

    #[test]
    fn test_rename_struct_field_for_shorthand() {
        mark::check!(test_rename_struct_field_for_shorthand);
        check(
            "j",
            r#"
struct Foo { i<|>: i32 }

impl Foo {
    fn new(i: i32) -> Self {
        Self { i }
    }
}
"#,
            r#"
struct Foo { j: i32 }

impl Foo {
    fn new(i: i32) -> Self {
        Self { j: i }
    }
}
"#,
        );
    }

    #[test]
    fn test_rename_local_for_field_shorthand() {
        mark::check!(test_rename_local_for_field_shorthand);
        check(
            "j",
            r#"
struct Foo { i: i32 }

impl Foo {
    fn new(i<|>: i32) -> Self {
        Self { i }
    }
}
"#,
            r#"
struct Foo { i: i32 }

impl Foo {
    fn new(j: i32) -> Self {
        Self { i: j }
    }
}
"#,
        );
    }

    #[test]
    fn test_field_shorthand_correct_struct() {
        check(
            "j",
            r#"
struct Foo { i<|>: i32 }
struct Bar { i: i32 }

impl Bar {
    fn new(i: i32) -> Self {
        Self { i }
    }
}
"#,
            r#"
struct Foo { j: i32 }
struct Bar { i: i32 }

impl Bar {
    fn new(i: i32) -> Self {
        Self { i }
    }
}
"#,
        );
    }

    #[test]
    fn test_shadow_local_for_struct_shorthand() {
        check(
            "j",
            r#"
struct Foo { i: i32 }

fn baz(i<|>: i32) -> Self {
     let x = Foo { i };
     {
         let i = 0;
         Foo { i }
     }
}
"#,
            r#"
struct Foo { i: i32 }

fn baz(j: i32) -> Self {
     let x = Foo { i: j };
     {
         let i = 0;
         Foo { i }
     }
}
"#,
        );
    }

    #[test]
    fn test_rename_mod() {
        check_expect(
            "foo2",
            r#"
//- /lib.rs
mod bar;

//- /bar.rs
mod foo<|>;

//- /bar/foo.rs
// empty
"#,
            expect![[r#"
                RangeInfo {
                    range: 4..7,
                    info: SourceChange {
                        source_file_edits: [
                            SourceFileEdit {
                                file_id: FileId(
                                    1,
                                ),
                                edit: TextEdit {
                                    indels: [
                                        Indel {
                                            insert: "foo2",
                                            delete: 4..7,
                                        },
                                    ],
                                },
                            },
                        ],
                        file_system_edits: [
                            MoveFile {
                                src: FileId(
                                    2,
                                ),
                                dst: AnchoredPathBuf {
                                    anchor: FileId(
                                        2,
                                    ),
                                    path: "foo2.rs",
                                },
                            },
                        ],
                        is_snippet: false,
                    },
                }
            "#]],
        );
    }

    #[test]
    fn test_rename_mod_in_use_tree() {
        check_expect(
            "quux",
            r#"
//- /main.rs
pub mod foo;
pub mod bar;
fn main() {}

//- /foo.rs
pub struct FooContent;

//- /bar.rs
use crate::foo<|>::FooContent;
"#,
            expect![[r#"
                RangeInfo {
                    range: 11..14,
                    info: SourceChange {
                        source_file_edits: [
                            SourceFileEdit {
                                file_id: FileId(
                                    0,
                                ),
                                edit: TextEdit {
                                    indels: [
                                        Indel {
                                            insert: "quux",
                                            delete: 8..11,
                                        },
                                    ],
                                },
                            },
                            SourceFileEdit {
                                file_id: FileId(
                                    2,
                                ),
                                edit: TextEdit {
                                    indels: [
                                        Indel {
                                            insert: "quux",
                                            delete: 11..14,
                                        },
                                    ],
                                },
                            },
                        ],
                        file_system_edits: [
                            MoveFile {
                                src: FileId(
                                    1,
                                ),
                                dst: AnchoredPathBuf {
                                    anchor: FileId(
                                        1,
                                    ),
                                    path: "quux.rs",
                                },
                            },
                        ],
                        is_snippet: false,
                    },
                }
            "#]],
        );
    }

    #[test]
    fn test_rename_mod_in_dir() {
        check_expect(
            "foo2",
            r#"
//- /lib.rs
mod fo<|>o;
//- /foo/mod.rs
// emtpy
"#,
            expect![[r#"
                RangeInfo {
                    range: 4..7,
                    info: SourceChange {
                        source_file_edits: [
                            SourceFileEdit {
                                file_id: FileId(
                                    0,
                                ),
                                edit: TextEdit {
                                    indels: [
                                        Indel {
                                            insert: "foo2",
                                            delete: 4..7,
                                        },
                                    ],
                                },
                            },
                        ],
                        file_system_edits: [
                            MoveFile {
                                src: FileId(
                                    1,
                                ),
                                dst: AnchoredPathBuf {
                                    anchor: FileId(
                                        1,
                                    ),
                                    path: "../foo2/mod.rs",
                                },
                            },
                        ],
                        is_snippet: false,
                    },
                }
            "#]],
        );
    }

    #[test]
    fn test_rename_unusually_nested_mod() {
        check_expect(
            "bar",
            r#"
//- /lib.rs
mod outer { mod fo<|>o; }

//- /outer/foo.rs
// emtpy
"#,
            expect![[r#"
                RangeInfo {
                    range: 16..19,
                    info: SourceChange {
                        source_file_edits: [
                            SourceFileEdit {
                                file_id: FileId(
                                    0,
                                ),
                                edit: TextEdit {
                                    indels: [
                                        Indel {
                                            insert: "bar",
                                            delete: 16..19,
                                        },
                                    ],
                                },
                            },
                        ],
                        file_system_edits: [
                            MoveFile {
                                src: FileId(
                                    1,
                                ),
                                dst: AnchoredPathBuf {
                                    anchor: FileId(
                                        1,
                                    ),
                                    path: "bar.rs",
                                },
                            },
                        ],
                        is_snippet: false,
                    },
                }
            "#]],
        );
    }

    #[test]
    fn test_module_rename_in_path() {
        check(
            "baz",
            r#"
mod <|>foo { pub fn bar() {} }

fn main() { foo::bar(); }
"#,
            r#"
mod baz { pub fn bar() {} }

fn main() { baz::bar(); }
"#,
        );
    }

    #[test]
    fn test_rename_mod_filename_and_path() {
        check_expect(
            "foo2",
            r#"
//- /lib.rs
mod bar;
fn f() {
    bar::foo::fun()
}

//- /bar.rs
pub mod foo<|>;

//- /bar/foo.rs
// pub fn fun() {}
"#,
            expect![[r#"
                RangeInfo {
                    range: 8..11,
                    info: SourceChange {
                        source_file_edits: [
                            SourceFileEdit {
                                file_id: FileId(
                                    1,
                                ),
                                edit: TextEdit {
                                    indels: [
                                        Indel {
                                            insert: "foo2",
                                            delete: 8..11,
                                        },
                                    ],
                                },
                            },
                            SourceFileEdit {
                                file_id: FileId(
                                    0,
                                ),
                                edit: TextEdit {
                                    indels: [
                                        Indel {
                                            insert: "foo2",
                                            delete: 27..30,
                                        },
                                    ],
                                },
                            },
                        ],
                        file_system_edits: [
                            MoveFile {
                                src: FileId(
                                    2,
                                ),
                                dst: AnchoredPathBuf {
                                    anchor: FileId(
                                        2,
                                    ),
                                    path: "foo2.rs",
                                },
                            },
                        ],
                        is_snippet: false,
                    },
                }
            "#]],
        );
    }

    #[test]
    fn test_enum_variant_from_module_1() {
        check(
            "Baz",
            r#"
mod foo {
    pub enum Foo { Bar<|> }
}

fn func(f: foo::Foo) {
    match f {
        foo::Foo::Bar => {}
    }
}
"#,
            r#"
mod foo {
    pub enum Foo { Baz }
}

fn func(f: foo::Foo) {
    match f {
        foo::Foo::Baz => {}
    }
}
"#,
        );
    }

    #[test]
    fn test_enum_variant_from_module_2() {
        check(
            "baz",
            r#"
mod foo {
    pub struct Foo { pub bar<|>: uint }
}

fn foo(f: foo::Foo) {
    let _ = f.bar;
}
"#,
            r#"
mod foo {
    pub struct Foo { pub baz: uint }
}

fn foo(f: foo::Foo) {
    let _ = f.baz;
}
"#,
        );
    }

    #[test]
    fn test_parameter_to_self() {
        check(
            "self",
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(foo<|>: &mut Foo) -> i32 {
        foo.i
    }
}
"#,
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(&mut self) -> i32 {
        self.i
    }
}
"#,
        );
        check(
            "self",
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(foo<|>: Foo) -> i32 {
        foo.i
    }
}
"#,
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(self) -> i32 {
        self.i
    }
}
"#,
        );
    }

    #[test]
    fn test_parameter_to_self_error_no_impl() {
        check(
            "self",
            r#"
struct Foo { i: i32 }

fn f(foo<|>: &mut Foo) -> i32 {
    foo.i
}
"#,
            "error: No impl block found for function",
        );
        check(
            "self",
            r#"
struct Foo { i: i32 }
struct Bar;

impl Bar {
    fn f(foo<|>: &mut Foo) -> i32 {
        foo.i
    }
}
"#,
            "error: Parameter type differs from impl block type",
        );
    }

    #[test]
    fn test_parameter_to_self_error_not_first() {
        check(
            "self",
            r#"
struct Foo { i: i32 }
impl Foo {
    fn f(x: (), foo<|>: &mut Foo) -> i32 {
        foo.i
    }
}
"#,
            "error: Only the first parameter can be self",
        );
    }

    #[test]
    fn test_parameter_to_self_impl_ref() {
        check(
            "self",
            r#"
struct Foo { i: i32 }
impl &Foo {
    fn f(foo<|>: &Foo) -> i32 {
        foo.i
    }
}
"#,
            r#"
struct Foo { i: i32 }
impl &Foo {
    fn f(self) -> i32 {
        self.i
    }
}
"#,
        );
    }

    #[test]
    fn test_self_to_parameter() {
        check(
            "foo",
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(&mut <|>self) -> i32 {
        self.i
    }
}
"#,
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(foo: &mut Foo) -> i32 {
        foo.i
    }
}
"#,
        );
    }

    #[test]
    fn test_owned_self_to_parameter() {
        check(
            "foo",
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(<|>self) -> i32 {
        self.i
    }
}
"#,
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(foo: Foo) -> i32 {
        foo.i
    }
}
"#,
        );
    }

    #[test]
    fn test_self_in_path_to_parameter() {
        check(
            "foo",
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(&self) -> i32 {
        let self_var = 1;
        self<|>.i
    }
}
"#,
            r#"
struct Foo { i: i32 }

impl Foo {
    fn f(foo: &Foo) -> i32 {
        let self_var = 1;
        foo.i
    }
}
"#,
        );
    }

    #[test]
    fn test_initializer_use_field_init_shorthand() {
        mark::check!(test_rename_field_expr_pat);
        check(
            "bar",
            r#"
struct Foo { i<|>: i32 }

fn foo(bar: i32) -> Foo {
    Foo { i: bar }
}
"#,
            r#"
struct Foo { bar: i32 }

fn foo(bar: i32) -> Foo {
    Foo { bar }
}
"#,
        );
    }

    #[test]
    fn test_struct_field_destructure_into_shorthand() {
        check(
            "baz",
            r#"
struct Foo { i<|>: i32 }

fn foo(foo: Foo) {
    let Foo { i: baz } = foo;
    let _ = baz;
}
"#,
            r#"
struct Foo { baz: i32 }

fn foo(foo: Foo) {
    let Foo { baz } = foo;
    let _ = baz;
}
"#,
        );
    }

    #[test]
    fn test_rename_binding_in_destructure_pat() {
        let expected_fixture = r#"
struct Foo {
    i: i32,
}

fn foo(foo: Foo) {
    let Foo { i: bar } = foo;
    let _ = bar;
}
"#;
        check(
            "bar",
            r#"
struct Foo {
    i: i32,
}

fn foo(foo: Foo) {
    let Foo { i: b } = foo;
    let _ = b<|>;
}
"#,
            expected_fixture,
        );
        check(
            "bar",
            r#"
struct Foo {
    i: i32,
}

fn foo(foo: Foo) {
    let Foo { i } = foo;
    let _ = i<|>;
}
"#,
            expected_fixture,
        );
    }

    #[test]
    fn test_rename_binding_in_destructure_param_pat() {
        check(
            "bar",
            r#"
struct Foo {
    i: i32
}

fn foo(Foo { i }: foo) -> i32 {
    i<|>
}
"#,
            r#"
struct Foo {
    i: i32
}

fn foo(Foo { i: bar }: foo) -> i32 {
    bar
}
"#,
        )
    }

    #[test]
    fn test_rename_lifetimes() {
        check(
            "'yeeee",
            r#"
trait Foo<'a> {
    fn foo() -> &'a ();
}
impl<'a> Foo<'a> for &'a () {
    fn foo() -> &'a<|> () {
        unimplemented!()
    }
}
"#,
            r#"
trait Foo<'a> {
    fn foo() -> &'a ();
}
impl<'yeeee> Foo<'yeeee> for &'yeeee () {
    fn foo() -> &'yeeee () {
        unimplemented!()
    }
}
"#,
        )
    }

    #[test]
    fn test_rename_bind_pat() {
        check(
            "new_name",
            r#"
fn main() {
    enum CustomOption<T> {
        None,
        Some(T),
    }

    let test_variable = CustomOption::Some(22);

    match test_variable {
        CustomOption::Some(foo<|>) if foo == 11 => {}
        _ => (),
    }
}"#,
            r#"
fn main() {
    enum CustomOption<T> {
        None,
        Some(T),
    }

    let test_variable = CustomOption::Some(22);

    match test_variable {
        CustomOption::Some(new_name) if new_name == 11 => {}
        _ => (),
    }
}"#,
        );
    }
}
