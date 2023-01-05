use lazy_static::lazy_static;
use snafu::prelude::*;
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use super::{
    error::*,
    inference::mkcref,
    inference::{CRef, Constrainable},
    schema::*,
    sql::get_rowtype,
    Compiler,
};
use crate::ast::SourceLocation;
use crate::runtime;
use crate::types::{AtomicType, Type};

pub trait Generic: Send + Sync + fmt::Debug {
    fn as_any(&self) -> &dyn Any;
    fn name(&self) -> &Ident;

    fn to_runtime_type(&self) -> runtime::error::Result<Type>;
    fn substitute(&self, variables: &BTreeMap<Ident, CRef<MType>>) -> Result<Arc<dyn Generic>>;

    fn unify(&self, other: &MType) -> Result<()>;
    fn get_rowtype(&self, _compiler: crate::compile::Compiler) -> Result<Option<CRef<MType>>> {
        Ok(None)
    }
}

pub trait GenericConstructor: Send + Sync {
    fn static_name() -> &'static Ident;
    fn new(loc: &SourceLocation, args: Vec<CRef<MType>>) -> Result<Arc<dyn Generic>>;
}

pub fn as_generic<T: Generic + 'static>(g: &dyn Generic) -> Option<&T> {
    g.as_any().downcast_ref::<T>()
}

fn debug_fmt_generic(
    f: &mut std::fmt::Formatter<'_>,
    name: &Ident,
    arg: &CRef<MType>,
) -> std::fmt::Result {
    write!(f, "{}<", name)?;
    std::fmt::Debug::fmt(arg, f)?;
    write!(f, ">")
}

pub struct SumGeneric(CRef<MType>);

fn validate_args(
    loc: &SourceLocation,
    args: &Vec<CRef<MType>>,
    num: usize,
    name: &Ident,
) -> Result<()> {
    if args.len() != num {
        return Err(CompileError::internal(
            loc.clone(),
            format!("{} expects {} argument", name, num).as_str(),
        ));
    }
    Ok(())
}

// TODO: Some of this boilerplate can be generated by a macro. We may need to create an implementation
// per tuple length...

lazy_static! {
    pub static ref SUM_GENERIC_NAME: Ident = "SumAgg".into();
    pub static ref EXTERNAL_GENERIC_NAME: Ident = "External".into();
    pub static ref GLOBAL_GENERICS: BTreeMap<Ident, Box<dyn GenericFactory>> = [
        BuiltinGeneric::<SumGeneric>::constructor(),
        BuiltinGeneric::<ExternalType>::constructor(),
    ]
    .into_iter()
    .map(|builder| (builder.name().clone(), builder))
    .collect::<BTreeMap<Ident, Box<dyn GenericFactory>>>();
}

impl SumGeneric {}

impl GenericConstructor for SumGeneric {
    fn new(loc: &SourceLocation, mut args: Vec<CRef<MType>>) -> Result<Arc<dyn Generic>> {
        validate_args(loc, &args, 1, Self::static_name())?;
        Ok(Arc::new(SumGeneric(args.swap_remove(0))))
    }

    fn static_name() -> &'static Ident {
        &SUM_GENERIC_NAME
    }
}

impl std::fmt::Debug for SumGeneric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_fmt_generic(f, Self::static_name(), &self.0)
    }
}

impl Generic for SumGeneric {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &Ident {
        Self::static_name()
    }

    fn to_runtime_type(&self) -> crate::runtime::error::Result<crate::types::Type> {
        let arg = self.0.must()?.read()?.to_runtime_type()?;

        // DuckDB's sum function follows the following rules:
        // 	sum(DECIMAL) -> DECIMAL
        //	sum(SMALLINT) -> HUGEINT
        //	sum(INTEGER) -> HUGEINT
        //	sum(BIGINT) -> HUGEINT
        //	sum(HUGEINT) -> HUGEINT
        //	sum(DOUBLE) -> DOUBLE
        match &arg {
            Type::Atom(at) => Ok(Type::Atom(match &at {
                AtomicType::Int8
                | AtomicType::Int16
                | AtomicType::Int32
                | AtomicType::Int64
                | AtomicType::UInt8
                | AtomicType::UInt16
                | AtomicType::UInt32
                | AtomicType::UInt64 => AtomicType::Decimal128(38, 0),
                AtomicType::Float32 | AtomicType::Float64 => AtomicType::Float64,
                AtomicType::Decimal128(..) | AtomicType::Decimal256(..) => at.clone(),
                _ => {
                    return Err(crate::runtime::error::RuntimeError::new(
                        format!(
                            "sum(): expected argument to be a numeric ype, got {:?}",
                            arg
                        )
                        .as_str(),
                    ))
                }
            })),
            _ => {
                return Err(crate::runtime::error::RuntimeError::new(
                    format!(
                        "sum(): expected argument to be an atomic type, got {:?}",
                        arg
                    )
                    .as_str(),
                ))
            }
        }
    }

    fn substitute(&self, variables: &BTreeMap<Ident, CRef<MType>>) -> Result<Arc<dyn Generic>> {
        Ok(Arc::new(Self(self.0.substitute(variables)?)))
    }

    fn unify(&self, other: &MType) -> Result<()> {
        // This is a bit of an approximate implementation, since it only works if the inner
        // type is known.
        if self.0.is_known()? {
            let final_type =
                MType::from_runtime_type(&self.to_runtime_type().context(RuntimeSnafu {
                    loc: ErrorLocation::Unknown,
                })?)?;
            other.unify(&final_type)?;
        }
        Ok(())
    }
}

pub struct ExternalType(CRef<MType>);

impl ExternalType {
    pub fn inner_type(&self) -> CRef<MType> {
        self.0.clone()
    }
}

impl GenericConstructor for ExternalType {
    fn new(loc: &SourceLocation, mut args: Vec<CRef<MType>>) -> Result<Arc<dyn Generic>> {
        validate_args(loc, &args, 1, Self::static_name())?;
        Ok(Arc::new(ExternalType(args.swap_remove(0))))
    }

    fn static_name() -> &'static Ident {
        &EXTERNAL_GENERIC_NAME
    }
}

impl std::fmt::Debug for ExternalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_fmt_generic(f, Self::static_name(), &self.0)
    }
}

impl Generic for ExternalType {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &Ident {
        Self::static_name()
    }

    fn to_runtime_type(&self) -> crate::runtime::error::Result<crate::types::Type> {
        self.0.must()?.read()?.to_runtime_type()
    }

    fn substitute(&self, variables: &BTreeMap<Ident, CRef<MType>>) -> Result<Arc<dyn Generic>> {
        Ok(Arc::new(Self(self.0.substitute(variables)?)))
    }

    fn unify(&self, other: &MType) -> Result<()> {
        let inner_type = &self.0;

        match other {
            MType::Generic(other_inner) => {
                if let Some(other) = as_generic::<Self>(other_inner.get().as_ref()) {
                    inner_type.unify(&other.0)?;
                } else {
                    inner_type.unify(&mkcref(other.clone()))?;
                }
            }
            other => {
                inner_type.unify(&mkcref(other.clone()))?;
            }
        };
        Ok(())
    }

    fn get_rowtype(&self, compiler: Compiler) -> Result<Option<CRef<MType>>> {
        Ok(Some(get_rowtype(compiler, self.0.clone())?))
    }
}

pub trait GenericFactory: Send + Sync {
    fn new(&self, loc: &SourceLocation, args: Vec<CRef<MType>>) -> Result<Arc<dyn Generic>>;
    fn name(&self) -> &Ident;
}

pub struct BuiltinGeneric<T: GenericConstructor>(std::marker::PhantomData<T>);

impl<T: GenericConstructor + 'static> BuiltinGeneric<T> {
    pub fn constructor() -> Box<dyn GenericFactory> {
        Box::new(BuiltinGeneric(std::marker::PhantomData::<T>)) as Box<dyn GenericFactory>
    }
}

impl<T: GenericConstructor> GenericFactory for BuiltinGeneric<T> {
    fn new(&self, loc: &SourceLocation, args: Vec<CRef<MType>>) -> Result<Arc<dyn Generic>> {
        T::new(loc, args)
    }

    fn name(&self) -> &Ident {
        T::static_name()
    }
}
