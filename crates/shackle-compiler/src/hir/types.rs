//! HIR representation of types written in a model (not computed types).
//!
//! See the `typecheck` module for computing types.

use super::{Expression, ItemData, Pattern};
pub use crate::syntax::ast::{OptType, PrimitiveType, VarType};
use crate::utils::arena::ArenaIndex;

/// Type of an expression
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Type {
	/// Primitive unbounded type
	Primitive {
		/// Inst
		inst: VarType,
		/// Optionality
		opt: OptType,
		/// The base type
		primitive_type: PrimitiveType,
	},
	/// Bounded type or type-inst alias
	Bounded {
		/// Inst
		inst: Option<VarType>,
		/// Optionality
		opt: Option<OptType>,
		/// The domain
		domain: ArenaIndex<Expression>,
	},
	/// Array type
	Array {
		/// Optionality
		opt: OptType,
		/// Type of dimensions
		dimensions: ArenaIndex<Type>,
		/// Type of element
		element: ArenaIndex<Type>,
	},
	/// Set type
	Set {
		/// Inst
		inst: VarType,
		/// Optionality
		opt: OptType,
		/// Type of element
		element: ArenaIndex<Type>,
	},
	/// Tuple type
	Tuple {
		/// Optionality
		opt: OptType,
		/// Tuple field types
		fields: Box<[ArenaIndex<Type>]>,
	},
	/// Record type
	Record {
		/// Optionality
		opt: OptType,
		/// Record field types
		fields: Box<[(ArenaIndex<Pattern>, ArenaIndex<Type>)]>,
	},
	/// Operation (function) type
	Operation {
		/// Optionality
		opt: OptType,
		/// Return type
		return_type: ArenaIndex<Type>,
		/// Parameter types
		parameter_types: Box<[ArenaIndex<Type>]>,
	},
	/// Anonymous type-inst var `_`
	AnonymousTypeInstVar {
		/// Inst to apply
		inst: Option<VarType>,
		/// Optionality to apply
		opt: Option<OptType>,
		/// The pattern for this type-inst var
		pattern: ArenaIndex<Pattern>,
	},
	/// Type inferred from RHS
	Any,

	/// Sentinel indicating an error during lowering
	Missing,
}

impl Type {
	/// Whether or not this type is completely known.
	///
	/// Returns `false` if the type contains an `Any` and `true` otherwise.
	pub fn is_complete(&self, data: &ItemData) -> bool {
		match self {
			Type::Any => false,
			Type::Primitive { .. }
			| Type::Bounded { .. }
			| Type::AnonymousTypeInstVar { .. }
			| Type::Missing => true,
			Type::Array {
				dimensions,
				element,
				..
			} => data[*dimensions].is_complete(data) && data[*element].is_complete(data),
			Type::Set { element, .. } => data[*element].is_complete(data),
			Type::Tuple { fields, .. } => fields.iter().all(|f| data[*f].is_complete(data)),
			Type::Record { fields, .. } => fields.iter().all(|(_, f)| data[*f].is_complete(data)),
			Type::Operation {
				return_type,
				parameter_types,
				..
			} => {
				data[*return_type].is_complete(data)
					&& parameter_types.iter().all(|p| data[*p].is_complete(data))
			}
		}
	}

	/// Return the `any` types in the given type.
	pub fn any_types(
		t: ArenaIndex<Type>,
		data: &ItemData,
	) -> impl '_ + Iterator<Item = ArenaIndex<Type>> {
		Type::walk(t, data).filter(|t| matches!(data[*t], Type::Any))
	}

	/// Return the anonymous type-inst variables in the given type.
	pub fn anonymous_ty_vars(
		t: ArenaIndex<Type>,
		data: &ItemData,
	) -> impl '_ + Iterator<Item = ArenaIndex<Type>> {
		Type::walk(t, data).filter(|t| matches!(data[*t], Type::AnonymousTypeInstVar { .. }))
	}

	/// Return the operation types in the given type.
	pub fn operations(
		t: ArenaIndex<Type>,
		data: &ItemData,
	) -> impl '_ + Iterator<Item = ArenaIndex<Type>> {
		Type::walk(t, data).filter(|t| matches!(data[*t], Type::Operation { .. }))
	}

	/// Get the unbounded primitive types in this type
	pub fn primitives(
		t: ArenaIndex<Type>,
		data: &ItemData,
	) -> impl '_ + Iterator<Item = ArenaIndex<Type>> {
		Type::walk(t, data).filter(|t| matches!(data[*t], Type::Primitive { .. }))
	}

	/// Get the expressions (bounds) contained in this type
	pub fn expressions(
		t: ArenaIndex<Type>,
		data: &ItemData,
	) -> impl '_ + Iterator<Item = ArenaIndex<Expression>> {
		Type::walk(t, data).filter_map(|t| {
			if let Type::Bounded { domain, .. } = data[t] {
				Some(domain)
			} else {
				None
			}
		})
	}

	/// Walk over the types contained in this type
	pub fn walk(
		t: ArenaIndex<Type>,
		data: &ItemData,
	) -> impl '_ + Iterator<Item = ArenaIndex<Type>> {
		let mut todo = vec![t];
		std::iter::from_fn(move || {
			let t = todo.pop()?;
			match &data[t] {
				Type::Array {
					dimensions,
					element,
					..
				} => {
					todo.push(*dimensions);
					todo.push(*element)
				}
				Type::Set { element, .. } => todo.push(*element),
				Type::Tuple { fields, .. } => todo.extend(fields.iter().copied()),
				Type::Record { fields, .. } => todo.extend(fields.iter().map(|(_, f)| *f)),
				Type::Operation {
					return_type,
					parameter_types,
					..
				} => {
					todo.push(*return_type);
					todo.extend(parameter_types.iter().copied());
				}
				_ => (),
			}
			Some(t)
		})
	}
}
