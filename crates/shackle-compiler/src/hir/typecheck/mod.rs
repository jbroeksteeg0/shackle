//! Typing and name resolution of MiniZinc programs.
//!
//! Performs
//! - Name resolution
//! - Overloading resolution
//! - Field access resolution
//! - Computation of types of items and expressions
//! - Type correctness check
//!
//! There are two entities which are typed: `signatures`, and `bodies`.
//!
//! Signatures are for any top-level items which can be referred to by an
//! identifier (e.g. functions, variable declarations). They only contain the
//! types relevant for computing the type of an identifier which refers to them.
//! That is, we don't care about the type of the RHS (unless it's an `any`
//! declaration).
//!
//! Bodies are for expressions in top-level items which need to be type-checked,
//! but cannot be referred to by an identifier. So computing types for bodies
//! may require signatures to be typed, and these in turn may require other
//! signatures to be typed, but never other bodies.
//!
//! Typing signatures does not cause types of signatures it depends on to be
//! queried from the database, as this can create cycles, which cannot be
//! recovered from in a useful way. The types of dependent signatures are
//! always computed, so there is some redundant recomputation, but the effect
//! is minimal.
//!
//! The `SignatureTypeContext` and `BodyTypeContext` structs implement the
//! `TypeContext` trait, which allows them to both use the `Typer` struct to
//! perform type-checking of expressions.

use std::{
	fmt::Write,
	ops::{Deref, Index},
	sync::Arc,
};

use super::{
	db::Hir,
	ids::{ExpressionRef, ItemRef, LocalItemRef, PatternRef},
	Expression, ItemData, Pattern,
};
use crate::{
	ty::{FunctionEntry, Ty, TyData, TyVar},
	utils::{
		arena::{ArenaIndex, ArenaMap},
		debug_print_strings, DebugPrint,
	},
	Error,
};

mod body;
mod signature;
mod toposort;
mod typer;

pub use self::{body::*, signature::*, toposort::*, typer::*};

/// Collected types for an item
///
/// This allows us to get the results of type computation in a particular item
/// by combining the computed types for the body along with its signature (if
/// it has one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeResult {
	item: ItemRef,
	body: Arc<BodyTypes>,
	signature: Option<Arc<SignatureTypes>>,
}

impl TypeResult {
	/// Get the computed types for this item
	pub fn new(db: &dyn Hir, item: ItemRef) -> Self {
		let it = item.local_item_ref(db);
		match it {
			LocalItemRef::Assignment(_) | LocalItemRef::Constraint(_) | LocalItemRef::Output(_) => {
				TypeResult {
					item,
					body: db.lookup_item_body(item),
					signature: None,
				}
			}
			_ => TypeResult {
				item,
				body: db.lookup_item_body(item),
				signature: Some(db.lookup_item_signature(item)),
			},
		}
	}

	/// Get the pattern this identifier expression resolves to
	pub fn name_resolution(&self, index: ArenaIndex<Expression>) -> Option<PatternRef> {
		if let Some(t) = self.body.identifier_resolution.get(&index) {
			return Some(*t);
		}
		if let Some(b) = &self.signature {
			if let Some(t) = b
				.identifier_resolution
				.get(&ExpressionRef::new(self.item, index))
			{
				return Some(*t);
			}
		}
		None
	}

	/// Get the pattern this pattern (e.g. enum atom/constructor) resolves to
	pub fn pattern_resolution(&self, index: ArenaIndex<Pattern>) -> Option<PatternRef> {
		if let Some(t) = self.body.pattern_resolution.get(&index) {
			return Some(*t);
		}
		if let Some(b) = &self.signature {
			if let Some(t) = b.pattern_resolution.get(&PatternRef::new(self.item, index)) {
				return Some(*t);
			}
		}
		None
	}

	/// Get the declaration for a pattern
	pub fn get_pattern(&self, pattern: ArenaIndex<Pattern>) -> Option<&PatternTy> {
		if let Some(d) = self.body.patterns.get(pattern) {
			return Some(d);
		}
		if let Some(b) = &self.signature {
			if let Some(d) = b.patterns.get(&PatternRef::new(self.item, pattern)) {
				return Some(d);
			}
		}
		None
	}

	/// Get the type of an expression
	pub fn get_expression(&self, expression: ArenaIndex<Expression>) -> Option<Ty> {
		if let Some(t) = self.body.expressions.get(expression) {
			return Some(*t);
		}
		if let Some(b) = &self.signature {
			if let Some(t) = b
				.expressions
				.get(&ExpressionRef::new(self.item, expression))
			{
				return Some(*t);
			}
		}
		None
	}

	/// Pretty print the type of an expression
	pub fn pretty_print_expression_ty(
		&self,
		db: &dyn Hir,
		data: &ItemData,
		expression: ArenaIndex<Expression>,
	) -> Option<String> {
		let ty = self.get_expression(expression)?;
		if let Expression::Identifier(i) = data[expression] {
			if let TyData::Function(opt, function) = ty.lookup(db.upcast()) {
				// Pretty print functions using item-like syntax if possible
				return Some(
					opt.pretty_print()
						.into_iter()
						.chain([function.pretty_print_item(db.upcast(), i)])
						.collect::<Vec<_>>()
						.join(" "),
				);
			}
		}
		Some(ty.pretty_print(db.upcast()))
	}

	/// Pretty print the type of a pattern
	pub fn pretty_print_pattern_ty(
		&self,
		db: &dyn Hir,
		data: &ItemData,
		pattern: ArenaIndex<Pattern>,
	) -> Option<String> {
		let decl = self.get_pattern(pattern)?;
		match decl {
			PatternTy::Variable(ty)
			| PatternTy::Argument(ty)
			| PatternTy::Enum(ty)
			| PatternTy::Destructuring(ty)
			| PatternTy::DestructuringFn {
				constructor: ty, ..
			} => {
				if let Pattern::Identifier(i) = data[pattern] {
					if let TyData::Function(opt, function) = ty.lookup(db.upcast()) {
						// Pretty print functions using item-like syntax if possible
						return Some(
							opt.pretty_print()
								.into_iter()
								.chain([function.pretty_print_item(db.upcast(), i)])
								.collect::<Vec<_>>()
								.join(" "),
						);
					}
					return Some(format!(
						"{}: {}",
						ty.pretty_print(db.upcast()),
						i.pretty_print(db)
					));
				}
				Some(ty.pretty_print(db.upcast()))
			}
			PatternTy::EnumAtom(ty) => Some(format!(
				"{}: {}",
				ty.pretty_print(db.upcast()),
				data[pattern].identifier()?.pretty_print(db)
			)),
			PatternTy::Function(f) => Some(
				f.overload
					.pretty_print_item(db.upcast(), data[pattern].identifier()?),
			),
			PatternTy::EnumConstructor(ec) => Some(
				ec.first()?
					.overload
					.pretty_print_item(db.upcast(), data[pattern].identifier()?),
			),
			PatternTy::TyVar(t) => Some(t.ty_var.pretty_print(db.upcast())),
			PatternTy::TypeAlias { ty, .. } => Some(format!(
				"type {} = {}",
				data[pattern].identifier()?.pretty_print(db),
				ty.pretty_print(db.upcast())
			)),
			_ => None,
		}
	}
}

impl Index<ArenaIndex<Pattern>> for TypeResult {
	type Output = PatternTy;
	fn index(&self, index: ArenaIndex<Pattern>) -> &Self::Output {
		self.get_pattern(index).expect("No declaration for pattern")
	}
}

impl Index<ArenaIndex<Expression>> for TypeResult {
	type Output = Ty;
	fn index(&self, index: ArenaIndex<Expression>) -> &Self::Output {
		if let Some(t) = self.body.expressions.get(index) {
			return t;
		}
		if let Some(b) = &self.signature {
			if let Some(t) = b.expressions.get(&ExpressionRef::new(self.item, index)) {
				return t;
			}
		}
		unreachable!("No type for expression {:?}", index)
	}
}

impl<'a> DebugPrint<'a> for TypeResult {
	type Database = dyn Hir + 'a;
	fn debug_print(&self, db: &Self::Database) -> String {
		let mut w = String::new();
		writeln!(&mut w, "Computed types:").unwrap();
		writeln!(&mut w, "  Declarations:").unwrap();
		for (i, t) in self
			.body
			.patterns
			.iter()
			.map(|(p, d)| (p, d))
			.chain(self.signature.iter().flat_map(|ts| {
				ts.patterns.iter().filter_map(|(p, d)| {
					if p.item() == self.item {
						Some((p.pattern(), d))
					} else {
						None
					}
				})
			}))
			.collect::<ArenaMap<_, _>>()
			.into_iter()
		{
			writeln!(&mut w, "    {:?}: {}", i, t.debug_print(db)).unwrap();
		}
		writeln!(&mut w, "  Expressions:").unwrap();
		for (i, e) in self
			.body
			.expressions
			.iter()
			.chain(self.signature.iter().flat_map(|ts| {
				ts.expressions.iter().filter_map(|(e, t)| {
					if e.item() == self.item {
						Some((e.expression(), t))
					} else {
						None
					}
				})
			}))
			.collect::<ArenaMap<_, _>>()
			.into_iter()
		{
			writeln!(&mut w, "    {:?}: {}", i, e.pretty_print(db.upcast())).unwrap();
		}
		writeln!(&mut w, "  Name resolution:").unwrap();
		for (i, p) in self
			.body
			.identifier_resolution
			.iter()
			.map(|(e, t)| (*e, t))
			.chain(self.signature.iter().flat_map(|ts| {
				ts.identifier_resolution.iter().filter_map(|(k, v)| {
					if k.item() == self.item {
						Some((k.expression(), v))
					} else {
						None
					}
				})
			}))
			.collect::<ArenaMap<_, _>>()
			.into_iter()
		{
			writeln!(&mut w, "    {:?}: {:?}", i, p).unwrap();
		}
		for (i, p) in self
			.body
			.pattern_resolution
			.iter()
			.map(|(e, t)| (*e, t))
			.chain(self.signature.iter().flat_map(|ts| {
				ts.pattern_resolution.iter().filter_map(|(k, v)| {
					if k.item() == self.item {
						Some((k.pattern(), v))
					} else {
						None
					}
				})
			}))
			.collect::<ArenaMap<_, _>>()
			.into_iter()
		{
			writeln!(&mut w, "    {:?}: {:?}", i, p).unwrap();
		}
		debug_print_strings(db, &w)
	}
}

/// Diagnostics collected when typing an item
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeDiagnostics(Arc<Vec<Error>>, Option<Arc<Vec<Error>>>);

impl TypeDiagnostics {
	/// Get the diagnostics for type-checking an item
	pub fn new(db: &dyn Hir, item: ItemRef) -> Self {
		let it = item.local_item_ref(db);
		match it {
			LocalItemRef::Assignment(_) | LocalItemRef::Constraint(_) | LocalItemRef::Output(_) => {
				TypeDiagnostics(db.lookup_item_body_errors(item), None)
			}
			_ => TypeDiagnostics(
				db.lookup_item_signature_errors(item),
				Some(db.lookup_item_body_errors(item)),
			),
		}
	}

	/// Iterate over the diagnostic vectors.
	///
	/// (Useful when using the `Diagnostics<Error>` helper)
	pub fn outer_iter(&self) -> impl '_ + Iterator<Item = Arc<Vec<Error>>> {
		[self.0.clone()].into_iter().chain(self.1.iter().cloned())
	}

	/// Iterate over the diagnostics
	pub fn iter(&self) -> impl Iterator<Item = &Error> {
		self.0.iter().chain(self.1.iter().flat_map(|es| es.iter()))
	}
}

/// Context for computation of types
///
/// The `Typer` calls these functions when computing types for expressions.
pub trait TypeContext {
	/// Add a declaration for a pattern
	fn add_declaration(&mut self, pattern: PatternRef, declaration: PatternTy);
	/// Add a type for an expression
	fn add_expression(&mut self, expression: ExpressionRef, ty: Ty);
	/// Add identifier resolution
	fn add_identifier_resolution(&mut self, expression: ExpressionRef, resolution: PatternRef);
	/// Add pattern resolution
	fn add_pattern_resolution(&mut self, pattern: PatternRef, resolution: PatternRef);
	/// Add an error
	fn add_diagnostic(&mut self, item: ItemRef, e: impl Into<Error>);

	/// Type a pattern (or lookup the type if already known)
	fn type_pattern(&mut self, db: &dyn Hir, pattern: PatternRef) -> PatternTy;
}

/// Get the signature of an item (ignores RHS of items except for `any` declarations)
pub fn collect_item_signature(
	db: &dyn Hir,
	item: ItemRef,
) -> (Arc<SignatureTypes>, Arc<Vec<Error>>) {
	log::debug!("Type checking signature of {:?}", item);
	let mut ctx = SignatureTypeContext::new(item);
	ctx.type_item(db, item);
	let (s, e) = ctx.finish();
	(Arc::new(s), Arc::new(e))
}

/// Type-check expressions in an item (other than those used in the signature)
pub fn collect_item_body(db: &dyn Hir, item: ItemRef) -> (Arc<BodyTypes>, Arc<Vec<Error>>) {
	log::debug!("Type checking body of {:?}", item);
	let model = item.model(db);
	let it = item.local_item_ref(db);
	let patterns = it.data(&model).patterns.len();
	let expressions = it.data(&model).expressions.len();
	let mut ctx = BodyTypeContext::with_capacity(item, patterns, expressions);
	ctx.type_item(db);
	let (s, e) = ctx.finish();
	(Arc::new(s), Arc::new(e))
}

/// Type of a pattern (usually a declaration)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PatternTy {
	/// Pattern is a variable declaration.
	Variable(Ty),
	/// Pattern is a function declaration.
	Function(Box<FunctionEntry>),
	/// Pattern is a function parameter.
	Argument(Ty),
	/// Pattern is a type-inst variable declaration.
	TyVar(TyVar),
	/// Pattern is a type-inst alias declaration.
	TypeAlias {
		/// The type which is aliased
		ty: Ty,
		/// True if this type alias contains a bounded type
		has_bounded: bool,
		/// True if this type alias contains a primitive type
		has_unbounded: bool,
	},
	/// An enum declaration (type is of the defining set of the enum).
	Enum(Ty),
	/// Enum constructor.
	///
	/// Defines the Foo(x) function.
	EnumConstructor(Box<[EnumConstructorEntry]>),
	/// Anonymous enum constructor.
	///
	/// While the constructor cannot actually be called,
	/// we still keep track of it for convenience.
	AnonymousEnumConstructor(Box<FunctionEntry>),
	/// Enum destructor.
	///
	/// Defines the Foo^-1(x) function.
	EnumDestructure(Box<[FunctionEntry]>),
	/// Enum atom
	EnumAtom(Ty),
	/// Annotation constructor.
	///
	/// Defines the Foo(x) function.
	AnnotationConstructor(Box<FunctionEntry>),
	/// Annotation destructor.
	///
	/// Defines the Foo^-1(x) function.
	AnnotationDestructure(Box<FunctionEntry>),
	/// Annotation atom
	AnnotationAtom,
	/// Destructuring pattern
	Destructuring(Ty),
	/// Destructuring function call identifier
	///
	/// Used for the constructor identifier pattern
	/// (the call will have the `Destructuring` type)
	DestructuringFn {
		/// The type of the constructor function
		constructor: Ty,
		/// The type of the destructor function
		destructor: Ty,
	},
	/// Currently computing - if encountered, indicates a cycle
	Computing,
}

impl<'a> DebugPrint<'a> for PatternTy {
	type Database = dyn Hir + 'a;

	fn debug_print(&self, db: &Self::Database) -> String {
		match self {
			PatternTy::Variable(ty) => format!("Variable({})", ty.pretty_print(db.upcast())),
			PatternTy::Function(function) => {
				format!("Function({})", function.overload.pretty_print(db.upcast()))
			}
			PatternTy::Argument(ty) => format!("Argument({})", ty.pretty_print(db.upcast())),
			PatternTy::TyVar(t) => format!("TyVar({})", t.ty_var.pretty_print(db.upcast())),
			PatternTy::TypeAlias { ty, .. } => {
				format!("TypeAlias({})", ty.pretty_print(db.upcast()))
			}
			PatternTy::Enum(ty) => format!("Enum({})", ty.pretty_print(db.upcast())),
			PatternTy::EnumConstructor(ecs) => {
				format!(
					"EnumConstructor({})",
					ecs.iter()
						.map(|f| f.overload.pretty_print(db.upcast()))
						.collect::<Vec<_>>()
						.join(", "),
				)
			}
			PatternTy::AnonymousEnumConstructor(f) => format!(
				"AnonymousEnumConstructor({})",
				f.overload.pretty_print(db.upcast()),
			),
			PatternTy::EnumDestructure(eds) => {
				format!(
					"EnumDestructure({})",
					eds.iter()
						.map(|f| f.overload.pretty_print(db.upcast()))
						.collect::<Vec<_>>()
						.join(", "),
				)
			}
			PatternTy::EnumAtom(ty) => format!("EnumAtom({})", ty.pretty_print(db.upcast())),
			PatternTy::AnnotationConstructor(f) => format!(
				"AnnotationConstructor({})",
				f.overload.pretty_print(db.upcast()),
			),
			PatternTy::AnnotationDestructure(f) => format!(
				"AnnotationDestructure({})",
				f.overload.pretty_print(db.upcast()),
			),
			PatternTy::AnnotationAtom => "AnnotationAtom".to_string(),
			PatternTy::Destructuring(ty) => {
				format!("Destructuring({})", ty.pretty_print(db.upcast()))
			}
			PatternTy::DestructuringFn {
				constructor,
				destructor,
			} => {
				format!(
					"DestructuringFn({}, {})",
					constructor.pretty_print(db.upcast()),
					destructor.pretty_print(db.upcast())
				)
			}
			PatternTy::Computing => "{computing}".to_owned(),
		}
	}
}

/// Constructor for an enum
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnumConstructorEntry {
	/// If true, this constructor is lifted and is not used for pattern matching
	pub is_lifted: bool,
	/// The function entry
	pub constructor: FunctionEntry,
}

impl Deref for EnumConstructorEntry {
	type Target = FunctionEntry;
	fn deref(&self) -> &Self::Target {
		&self.constructor
	}
}

#[cfg(test)]
mod test;
