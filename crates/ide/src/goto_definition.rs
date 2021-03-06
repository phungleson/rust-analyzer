use either::Either;
use hir::Semantics;
use ide_db::{
    base_db::FileId,
    defs::{NameClass, NameRefClass},
    symbol_index, RootDatabase,
};
use syntax::{ast, match_ast, AstNode, SyntaxKind::*, SyntaxToken, TokenAtOffset, T};

use crate::{
    display::{ToNav, TryToNav},
    FilePosition, NavigationTarget, RangeInfo, SymbolKind,
};

// Feature: Go to Definition
//
// Navigates to the definition of an identifier.
//
// |===
// | Editor  | Shortcut
//
// | VS Code | kbd:[F12]
// |===
pub(crate) fn goto_definition(
    db: &RootDatabase,
    position: FilePosition,
) -> Option<RangeInfo<Vec<NavigationTarget>>> {
    let sema = Semantics::new(db);
    let file = sema.parse(position.file_id).syntax().clone();
    let original_token = pick_best(file.token_at_offset(position.offset))?;
    let token = sema.descend_into_macros(original_token.clone());
    let parent = token.parent();

    let nav_targets = match_ast! {
        match parent {
            ast::NameRef(name_ref) => {
                reference_definition(&sema, Either::Right(&name_ref)).to_vec()
            },
            ast::Name(name) => {
                let def = NameClass::classify(&sema, &name)?.referenced_or_defined(sema.db);
                let nav = def.try_to_nav(sema.db)?;
                vec![nav]
            },
            ast::SelfParam(self_param) => {
                vec![self_to_nav_target(self_param, position.file_id)?]
            },
            ast::PathSegment(segment) => {
                segment.self_token()?;
                let path = segment.parent_path();
                if path.qualifier().is_some() && !ast::PathExpr::can_cast(path.syntax().parent()?.kind()) {
                    return None;
                }
                let func = segment.syntax().ancestors().find_map(ast::Fn::cast)?;
                let self_param = func.param_list()?.self_param()?;
                vec![self_to_nav_target(self_param, position.file_id)?]
            },
            ast::Lifetime(lt) => if let Some(name_class) = NameClass::classify_lifetime(&sema, &lt) {
                let def = name_class.referenced_or_defined(sema.db);
                let nav = def.try_to_nav(sema.db)?;
                vec![nav]
            } else {
                reference_definition(&sema, Either::Left(&lt)).to_vec()
            },
            _ => return None,
        }
    };

    Some(RangeInfo::new(original_token.text_range(), nav_targets))
}

fn pick_best(tokens: TokenAtOffset<SyntaxToken>) -> Option<SyntaxToken> {
    return tokens.max_by_key(priority);
    fn priority(n: &SyntaxToken) -> usize {
        match n.kind() {
            IDENT | INT_NUMBER | LIFETIME_IDENT | T![self] => 2,
            kind if kind.is_trivia() => 0,
            _ => 1,
        }
    }
}

fn self_to_nav_target(self_param: ast::SelfParam, file_id: FileId) -> Option<NavigationTarget> {
    let self_token = self_param.self_token()?;
    Some(NavigationTarget {
        file_id,
        full_range: self_param.syntax().text_range(),
        focus_range: Some(self_token.text_range()),
        name: self_token.text().clone(),
        kind: Some(SymbolKind::SelfParam),
        container_name: None,
        description: None,
        docs: None,
    })
}

#[derive(Debug)]
pub(crate) enum ReferenceResult {
    Exact(NavigationTarget),
    Approximate(Vec<NavigationTarget>),
}

impl ReferenceResult {
    fn to_vec(self) -> Vec<NavigationTarget> {
        match self {
            ReferenceResult::Exact(target) => vec![target],
            ReferenceResult::Approximate(vec) => vec,
        }
    }
}

pub(crate) fn reference_definition(
    sema: &Semantics<RootDatabase>,
    name_ref: Either<&ast::Lifetime, &ast::NameRef>,
) -> ReferenceResult {
    let name_kind = name_ref.either(
        |lifetime| NameRefClass::classify_lifetime(sema, lifetime),
        |name_ref| NameRefClass::classify(sema, name_ref),
    );
    if let Some(def) = name_kind {
        let def = def.referenced(sema.db);
        return match def.try_to_nav(sema.db) {
            Some(nav) => ReferenceResult::Exact(nav),
            None => ReferenceResult::Approximate(Vec::new()),
        };
    }

    // Fallback index based approach:
    let name = name_ref.either(ast::Lifetime::text, ast::NameRef::text);
    let navs =
        symbol_index::index_resolve(sema.db, name).into_iter().map(|s| s.to_nav(sema.db)).collect();
    ReferenceResult::Approximate(navs)
}

#[cfg(test)]
mod tests {
    use ide_db::base_db::FileRange;
    use syntax::{TextRange, TextSize};

    use crate::fixture;

    fn check(ra_fixture: &str) {
        let (analysis, position, mut annotations) = fixture::annotations(ra_fixture);
        let (mut expected, data) = annotations.pop().unwrap();
        match data.as_str() {
            "" => (),
            "file" => {
                expected.range =
                    TextRange::up_to(TextSize::of(&*analysis.file_text(expected.file_id).unwrap()))
            }
            data => panic!("bad data: {}", data),
        }

        let mut navs =
            analysis.goto_definition(position).unwrap().expect("no definition found").info;
        if navs.len() == 0 {
            panic!("unresolved reference")
        }
        assert_eq!(navs.len(), 1);

        let nav = navs.pop().unwrap();
        assert_eq!(expected, FileRange { file_id: nav.file_id, range: nav.focus_or_full_range() });
    }

    #[test]
    fn goto_def_for_extern_crate() {
        check(
            r#"
            //- /main.rs crate:main deps:std
            extern crate std<|>;
            //- /std/lib.rs crate:std
            // empty
            //^ file
            "#,
        )
    }

    #[test]
    fn goto_def_for_renamed_extern_crate() {
        check(
            r#"
            //- /main.rs crate:main deps:std
            extern crate std as abc<|>;
            //- /std/lib.rs crate:std
            // empty
            //^ file
            "#,
        )
    }

    #[test]
    fn goto_def_in_items() {
        check(
            r#"
struct Foo;
     //^^^
enum E { X(Foo<|>) }
"#,
        );
    }

    #[test]
    fn goto_def_at_start_of_item() {
        check(
            r#"
struct Foo;
     //^^^
enum E { X(<|>Foo) }
"#,
        );
    }

    #[test]
    fn goto_definition_resolves_correct_name() {
        check(
            r#"
//- /lib.rs
use a::Foo;
mod a;
mod b;
enum E { X(Foo<|>) }

//- /a.rs
struct Foo;
     //^^^
//- /b.rs
struct Foo;
"#,
        );
    }

    #[test]
    fn goto_def_for_module_declaration() {
        check(
            r#"
//- /lib.rs
mod <|>foo;

//- /foo.rs
// empty
//^ file
"#,
        );

        check(
            r#"
//- /lib.rs
mod <|>foo;

//- /foo/mod.rs
// empty
//^ file
"#,
        );
    }

    #[test]
    fn goto_def_for_macros() {
        check(
            r#"
macro_rules! foo { () => { () } }
           //^^^
fn bar() {
    <|>foo!();
}
"#,
        );
    }

    #[test]
    fn goto_def_for_macros_from_other_crates() {
        check(
            r#"
//- /lib.rs
use foo::foo;
fn bar() {
    <|>foo!();
}

//- /foo/lib.rs
#[macro_export]
macro_rules! foo { () => { () } }
           //^^^
"#,
        );
    }

    #[test]
    fn goto_def_for_macros_in_use_tree() {
        check(
            r#"
//- /lib.rs
use foo::foo<|>;

//- /foo/lib.rs
#[macro_export]
macro_rules! foo { () => { () } }
           //^^^
"#,
        );
    }

    #[test]
    fn goto_def_for_macro_defined_fn_with_arg() {
        check(
            r#"
//- /lib.rs
macro_rules! define_fn {
    ($name:ident) => (fn $name() {})
}

define_fn!(foo);
         //^^^

fn bar() {
   <|>foo();
}
"#,
        );
    }

    #[test]
    fn goto_def_for_macro_defined_fn_no_arg() {
        check(
            r#"
//- /lib.rs
macro_rules! define_fn {
    () => (fn foo() {})
}

  define_fn!();
//^^^^^^^^^^^^^

fn bar() {
   <|>foo();
}
"#,
        );
    }

    #[test]
    fn goto_definition_works_for_macro_inside_pattern() {
        check(
            r#"
//- /lib.rs
macro_rules! foo {() => {0}}
           //^^^

fn bar() {
    match (0,1) {
        (<|>foo!(), _) => {}
    }
}
"#,
        );
    }

    #[test]
    fn goto_definition_works_for_macro_inside_match_arm_lhs() {
        check(
            r#"
//- /lib.rs
macro_rules! foo {() => {0}}
           //^^^
fn bar() {
    match 0 {
        <|>foo!() => {}
    }
}
"#,
        );
    }

    #[test]
    fn goto_def_for_use_alias() {
        check(
            r#"
//- /lib.rs crate:main deps:foo
use foo as bar<|>;

//- /foo/lib.rs crate:foo
// empty
//^ file
"#,
        );
    }

    #[test]
    fn goto_def_for_use_alias_foo_macro() {
        check(
            r#"
//- /lib.rs crate:main deps:foo
use foo::foo as bar<|>;

//- /foo/lib.rs crate:foo
#[macro_export]
macro_rules! foo { () => { () } }
           //^^^
"#,
        );
    }

    #[test]
    fn goto_def_for_methods() {
        check(
            r#"
struct Foo;
impl Foo {
    fn frobnicate(&self) { }
     //^^^^^^^^^^
}

fn bar(foo: &Foo) {
    foo.frobnicate<|>();
}
"#,
        );
    }

    #[test]
    fn goto_def_for_fields() {
        check(
            r#"
struct Foo {
    spam: u32,
} //^^^^

fn bar(foo: &Foo) {
    foo.spam<|>;
}
"#,
        );
    }

    #[test]
    fn goto_def_for_record_fields() {
        check(
            r#"
//- /lib.rs
struct Foo {
    spam: u32,
} //^^^^

fn bar() -> Foo {
    Foo {
        spam<|>: 0,
    }
}
"#,
        );
    }

    #[test]
    fn goto_def_for_record_pat_fields() {
        check(
            r#"
//- /lib.rs
struct Foo {
    spam: u32,
} //^^^^

fn bar(foo: Foo) -> Foo {
    let Foo { spam<|>: _, } = foo
}
"#,
        );
    }

    #[test]
    fn goto_def_for_record_fields_macros() {
        check(
            r"
macro_rules! m { () => { 92 };}
struct Foo { spam: u32 }
           //^^^^

fn bar() -> Foo {
    Foo { spam<|>: m!() }
}
",
        );
    }

    #[test]
    fn goto_for_tuple_fields() {
        check(
            r#"
struct Foo(u32);
         //^^^

fn bar() {
    let foo = Foo(0);
    foo.<|>0;
}
"#,
        );
    }

    #[test]
    fn goto_def_for_ufcs_inherent_methods() {
        check(
            r#"
struct Foo;
impl Foo {
    fn frobnicate() { }
}    //^^^^^^^^^^

fn bar(foo: &Foo) {
    Foo::frobnicate<|>();
}
"#,
        );
    }

    #[test]
    fn goto_def_for_ufcs_trait_methods_through_traits() {
        check(
            r#"
trait Foo {
    fn frobnicate();
}    //^^^^^^^^^^

fn bar() {
    Foo::frobnicate<|>();
}
"#,
        );
    }

    #[test]
    fn goto_def_for_ufcs_trait_methods_through_self() {
        check(
            r#"
struct Foo;
trait Trait {
    fn frobnicate();
}    //^^^^^^^^^^
impl Trait for Foo {}

fn bar() {
    Foo::frobnicate<|>();
}
"#,
        );
    }

    #[test]
    fn goto_definition_on_self() {
        check(
            r#"
struct Foo;
impl Foo {
   //^^^
    pub fn new() -> Self {
        Self<|> {}
    }
}
"#,
        );
        check(
            r#"
struct Foo;
impl Foo {
   //^^^
    pub fn new() -> Self<|> {
        Self {}
    }
}
"#,
        );

        check(
            r#"
enum Foo { A }
impl Foo {
   //^^^
    pub fn new() -> Self<|> {
        Foo::A
    }
}
"#,
        );

        check(
            r#"
enum Foo { A }
impl Foo {
   //^^^
    pub fn thing(a: &Self<|>) {
    }
}
"#,
        );
    }

    #[test]
    fn goto_definition_on_self_in_trait_impl() {
        check(
            r#"
struct Foo;
trait Make {
    fn new() -> Self;
}
impl Make for Foo {
            //^^^
    fn new() -> Self {
        Self<|> {}
    }
}
"#,
        );

        check(
            r#"
struct Foo;
trait Make {
    fn new() -> Self;
}
impl Make for Foo {
            //^^^
    fn new() -> Self<|> {
        Self {}
    }
}
"#,
        );
    }

    #[test]
    fn goto_def_when_used_on_definition_name_itself() {
        check(
            r#"
struct Foo<|> { value: u32 }
     //^^^
            "#,
        );

        check(
            r#"
struct Foo {
    field<|>: string,
} //^^^^^
"#,
        );

        check(
            r#"
fn foo_test<|>() { }
 //^^^^^^^^
"#,
        );

        check(
            r#"
enum Foo<|> { Variant }
   //^^^
"#,
        );

        check(
            r#"
enum Foo {
    Variant1,
    Variant2<|>,
  //^^^^^^^^
    Variant3,
}
"#,
        );

        check(
            r#"
static INNER<|>: &str = "";
     //^^^^^
"#,
        );

        check(
            r#"
const INNER<|>: &str = "";
    //^^^^^
"#,
        );

        check(
            r#"
type Thing<|> = Option<()>;
   //^^^^^
"#,
        );

        check(
            r#"
trait Foo<|> { }
    //^^^
"#,
        );

        check(
            r#"
mod bar<|> { }
  //^^^
"#,
        );
    }

    #[test]
    fn goto_from_macro() {
        check(
            r#"
macro_rules! id {
    ($($tt:tt)*) => { $($tt)* }
}
fn foo() {}
 //^^^
id! {
    fn bar() {
        fo<|>o();
    }
}
mod confuse_index { fn foo(); }
"#,
        );
    }

    #[test]
    fn goto_through_format() {
        check(
            r#"
#[macro_export]
macro_rules! format {
    ($($arg:tt)*) => ($crate::fmt::format($crate::__export::format_args!($($arg)*)))
}
#[rustc_builtin_macro]
#[macro_export]
macro_rules! format_args {
    ($fmt:expr) => ({ /* compiler built-in */ });
    ($fmt:expr, $($args:tt)*) => ({ /* compiler built-in */ })
}
pub mod __export {
    pub use crate::format_args;
    fn foo() {} // for index confusion
}
fn foo() -> i8 {}
 //^^^
fn test() {
    format!("{}", fo<|>o())
}
"#,
        );
    }

    #[test]
    fn goto_for_type_param() {
        check(
            r#"
struct Foo<T: Clone> { t: <|>T }
         //^
"#,
        );
    }

    #[test]
    fn goto_within_macro() {
        check(
            r#"
macro_rules! id {
    ($($tt:tt)*) => ($($tt)*)
}

fn foo() {
    let x = 1;
      //^
    id!({
        let y = <|>x;
        let z = y;
    });
}
"#,
        );

        check(
            r#"
macro_rules! id {
    ($($tt:tt)*) => ($($tt)*)
}

fn foo() {
    let x = 1;
    id!({
        let y = x;
          //^
        let z = <|>y;
    });
}
"#,
        );
    }

    #[test]
    fn goto_def_in_local_fn() {
        check(
            r#"
fn main() {
    fn foo() {
        let x = 92;
          //^
        <|>x;
    }
}
"#,
        );
    }

    #[test]
    fn goto_def_in_local_macro() {
        check(
            r#"
fn bar() {
    macro_rules! foo { () => { () } }
               //^^^
    <|>foo!();
}
"#,
        );
    }

    #[test]
    fn goto_def_for_field_init_shorthand() {
        check(
            r#"
struct Foo { x: i32 }
fn main() {
    let x = 92;
      //^
    Foo { x<|> };
}
"#,
        )
    }

    #[test]
    fn goto_def_for_enum_variant_field() {
        check(
            r#"
enum Foo {
    Bar { x: i32 }
}       //^
fn baz(foo: Foo) {
    match foo {
        Foo::Bar { x<|> } => x
    };
}
"#,
        );
    }

    #[test]
    fn goto_def_for_enum_variant_self_pattern_const() {
        check(
            r#"
enum Foo { Bar }
         //^^^
impl Foo {
    fn baz(self) {
        match self { Self::Bar<|> => {} }
    }
}
"#,
        );
    }

    #[test]
    fn goto_def_for_enum_variant_self_pattern_record() {
        check(
            r#"
enum Foo { Bar { val: i32 } }
         //^^^
impl Foo {
    fn baz(self) -> i32 {
        match self { Self::Bar<|> { val } => {} }
    }
}
"#,
        );
    }

    #[test]
    fn goto_def_for_enum_variant_self_expr_const() {
        check(
            r#"
enum Foo { Bar }
         //^^^
impl Foo {
    fn baz(self) { Self::Bar<|>; }
}
"#,
        );
    }

    #[test]
    fn goto_def_for_enum_variant_self_expr_record() {
        check(
            r#"
enum Foo { Bar { val: i32 } }
         //^^^
impl Foo {
    fn baz(self) { Self::Bar<|> {val: 4}; }
}
"#,
        );
    }

    #[test]
    fn goto_def_for_type_alias_generic_parameter() {
        check(
            r#"
type Alias<T> = T<|>;
         //^
"#,
        )
    }

    #[test]
    fn goto_def_for_macro_container() {
        check(
            r#"
//- /lib.rs
foo::module<|>::mac!();

//- /foo/lib.rs
pub mod module {
      //^^^^^^
    #[macro_export]
    macro_rules! _mac { () => { () } }
    pub use crate::_mac as mac;
}
"#,
        );
    }

    #[test]
    fn goto_def_for_assoc_ty_in_path() {
        check(
            r#"
trait Iterator {
    type Item;
       //^^^^
}

fn f() -> impl Iterator<Item<|> = u8> {}
"#,
        );
    }

    #[test]
    fn goto_def_for_assoc_ty_in_path_multiple() {
        check(
            r#"
trait Iterator {
    type A;
       //^
    type B;
}

fn f() -> impl Iterator<A<|> = u8, B = ()> {}
"#,
        );
        check(
            r#"
trait Iterator {
    type A;
    type B;
       //^
}

fn f() -> impl Iterator<A = u8, B<|> = ()> {}
"#,
        );
    }

    #[test]
    fn goto_def_for_assoc_ty_ufcs() {
        check(
            r#"
trait Iterator {
    type Item;
       //^^^^
}

fn g() -> <() as Iterator<Item<|> = ()>>::Item {}
"#,
        );
    }

    #[test]
    fn goto_def_for_assoc_ty_ufcs_multiple() {
        check(
            r#"
trait Iterator {
    type A;
       //^
    type B;
}

fn g() -> <() as Iterator<A<|> = (), B = u8>>::B {}
"#,
        );
        check(
            r#"
trait Iterator {
    type A;
    type B;
       //^
}

fn g() -> <() as Iterator<A = (), B<|> = u8>>::A {}
"#,
        );
    }

    #[test]
    fn goto_self_param_ty_specified() {
        check(
            r#"
struct Foo {}

impl Foo {
    fn bar(self: &Foo) {
         //^^^^
        let foo = sel<|>f;
    }
}"#,
        )
    }

    #[test]
    fn goto_self_param_on_decl() {
        check(
            r#"
struct Foo {}

impl Foo {
    fn bar(&self<|>) {
          //^^^^
    }
}"#,
        )
    }

    #[test]
    fn goto_lifetime_param_on_decl() {
        check(
            r#"
fn foo<'foobar<|>>(_: &'foobar ()) {
     //^^^^^^^
}"#,
        )
    }

    #[test]
    fn goto_lifetime_param_decl() {
        check(
            r#"
fn foo<'foobar>(_: &'foobar<|> ()) {
     //^^^^^^^
}"#,
        )
    }

    #[test]
    fn goto_lifetime_param_decl_nested() {
        check(
            r#"
fn foo<'foobar>(_: &'foobar ()) {
    fn foo<'foobar>(_: &'foobar<|> ()) {}
         //^^^^^^^
}"#,
        )
    }

    #[test]
    #[ignore] // requires the HIR to somehow track these hrtb lifetimes
    fn goto_lifetime_hrtb() {
        check(
            r#"trait Foo<T> {}
fn foo<T>() where for<'a> T: Foo<&'a<|> (u8, u16)>, {}
                    //^^
"#,
        );
        check(
            r#"trait Foo<T> {}
fn foo<T>() where for<'a<|>> T: Foo<&'a (u8, u16)>, {}
                    //^^
"#,
        );
    }

    #[test]
    #[ignore] // requires ForTypes to be implemented
    fn goto_lifetime_hrtb_for_type() {
        check(
            r#"trait Foo<T> {}
fn foo<T>() where T: for<'a> Foo<&'a<|> (u8, u16)>, {}
                       //^^
"#,
        );
    }
}
