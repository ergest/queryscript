pub use datafusion::arrow::datatypes::DataType as ArrowDataType;
use sqlparser::ast as sqlast;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::ast;
use crate::compile::{
    error::{CompileError, Result},
    inference::{mkcref, Constrainable, Constrained},
};
use crate::runtime;
use crate::types::{AtomicType, Field, FnType, Type};

pub use crate::compile::inference::CRef;

pub type Ident = ast::Ident;

#[derive(Debug, Clone)]
pub struct MFnType {
    pub args: Vec<MField>,
    pub ret: CRef<MType>,
}

#[derive(Debug, Clone)]
pub struct MField {
    pub name: String,
    pub type_: CRef<MType>,
    pub nullable: bool,
}

impl MField {
    pub fn new_nullable(name: String, type_: CRef<MType>) -> MField {
        MField {
            name,
            type_,
            nullable: true,
        }
    }
}

#[derive(Clone)]
pub enum MType {
    Atom(AtomicType),
    Record(Vec<MField>),
    List(CRef<MType>),
    Fn(MFnType),
    Name(String),
}

impl MType {
    pub fn new_unknown(debug_name: &str) -> CRef<MType> {
        CRef::new_unknown(debug_name)
    }

    pub fn to_runtime_type(&self) -> runtime::error::Result<Type> {
        match self {
            MType::Atom(a) => Ok(Type::Atom(a.clone())),
            MType::Record(fields) => Ok(Type::Record(
                fields
                    .iter()
                    .map(|f| {
                        Ok(Field {
                            name: f.name.clone(),
                            type_: f.type_.must()?.read()?.to_runtime_type()?,
                            nullable: f.nullable,
                        })
                    })
                    .collect::<runtime::error::Result<Vec<_>>>()?,
            )),
            MType::List(inner) => Ok(Type::List(Box::new(
                inner.must()?.read()?.to_runtime_type()?,
            ))),
            MType::Fn(MFnType { args, ret }) => Ok(Type::Fn(FnType {
                args: args
                    .iter()
                    .map(|a| {
                        Ok(Field {
                            name: a.name.clone(),
                            type_: a.type_.must()?.read()?.to_runtime_type()?,
                            nullable: a.nullable,
                        })
                    })
                    .collect::<runtime::error::Result<Vec<_>>>()?,
                ret: Box::new(ret.must()?.read()?.to_runtime_type()?),
            })),
            MType::Name(_) => {
                runtime::error::fail!("Unresolved type name cannot exist at runtime: {:?}", self)
            }
        }
    }

    pub fn from_runtime_type(type_: &Type) -> Result<MType> {
        match type_ {
            Type::Atom(a) => Ok(MType::Atom(a.clone())),
            Type::Record(fields) => Ok(MType::Record(
                fields
                    .iter()
                    .map(|f| {
                        Ok(MField {
                            name: f.name.clone(),
                            type_: mkcref(MType::from_runtime_type(&f.type_)?),
                            nullable: f.nullable,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            )),
            Type::List(inner) => Ok(MType::List(mkcref(MType::from_runtime_type(&inner)?))),
            Type::Fn(FnType { args, ret }) => Ok(MType::Fn(MFnType {
                args: args
                    .iter()
                    .map(|a| {
                        Ok(MField {
                            name: a.name.clone(),
                            type_: mkcref(MType::from_runtime_type(&a.type_)?),
                            nullable: a.nullable,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                ret: mkcref(MType::from_runtime_type(&ret)?),
            })),
        }
    }

    pub fn substitute(&self, variables: &BTreeMap<String, CRef<MType>>) -> Result<CRef<MType>> {
        let type_ = match self {
            MType::Atom(a) => mkcref(MType::Atom(a.clone())),
            MType::Record(fields) => mkcref(MType::Record(
                fields
                    .iter()
                    .map(|f| {
                        Ok(MField {
                            name: f.name.clone(),
                            type_: f.type_.substitute(variables)?,
                            nullable: f.nullable,
                        })
                    })
                    .collect::<Result<_>>()?,
            )),
            MType::List(i) => mkcref(MType::List(i.substitute(variables)?)),
            MType::Fn(MFnType { args, ret }) => mkcref(MType::Fn(MFnType {
                args: args
                    .iter()
                    .map(|a| {
                        Ok(MField {
                            name: a.name.clone(),
                            type_: a.type_.substitute(variables)?,
                            nullable: a.nullable,
                        })
                    })
                    .collect::<Result<_>>()?,
                ret: ret.substitute(variables)?,
            })),
            MType::Name(n) => variables
                .get(n)
                .ok_or_else(|| CompileError::no_such_entry(vec![n.clone()]))?
                .clone(),
        };

        Ok(type_)
    }
}

#[derive(Clone, Debug)]
pub struct CTypedExpr {
    pub type_: CRef<MType>,
    pub expr: CRef<Expr<CRef<MType>>>,
}

impl CTypedExpr {
    pub fn to_runtime_type(&self) -> runtime::error::Result<TypedExpr<Ref<Type>>> {
        Ok(TypedExpr {
            type_: mkref(self.type_.must()?.read()?.to_runtime_type()?),
            expr: Arc::new(self.expr.must()?.read()?.to_runtime_type()?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct CTypedNameAndExpr {
    pub name: String,
    pub type_: CRef<MType>,
    pub expr: CRef<Expr<CRef<MType>>>,
}

struct DebugMFields<'a>(&'a Vec<MField>);

impl<'a> fmt::Debug for DebugMFields<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for i in 0..self.0.len() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(self.0[i].name.as_str())?;
            f.write_str(" ")?;
            self.0[i].type_.fmt(f)?;
            if !self.0[i].nullable {
                f.write_str(" not null")?;
            }
        }
        f.write_str("}")?;
        Ok(())
    }
}

impl fmt::Debug for MType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MType::Atom(atom) => atom.fmt(f)?,
            MType::Record(fields) => DebugMFields(fields).fmt(f)?,
            MType::List(inner) => {
                f.write_str("[")?;
                inner.fmt(f)?;
                f.write_str("]")?;
            }
            MType::Fn(func) => {
                f.write_str("λ ")?;
                DebugMFields(&func.args).fmt(f)?;
                f.write_str(" -> ")?;
                func.ret.fmt(f)?;
            }
            MType::Name(n) => n.fmt(f)?,
        }
        Ok(())
    }
}

impl Constrainable for MType {
    fn unify(&self, other: &MType) -> Result<()> {
        match self {
            MType::Atom(la) => match other {
                MType::Atom(ra) => {
                    if la != ra {
                        return Err(CompileError::wrong_type(self, other));
                    }
                }
                _ => return Err(CompileError::wrong_type(self, other)),
            },
            MType::Record(lfields) => match other {
                MType::Record(rfields) => lfields.unify(rfields)?,
                _ => return Err(CompileError::wrong_type(self, other)),
            },
            MType::List(linner) => match other {
                MType::List(rinner) => linner.unify(rinner)?,
                _ => return Err(CompileError::wrong_type(self, other)),
            },
            MType::Fn(MFnType {
                args: largs,
                ret: lret,
            }) => match other {
                MType::Fn(MFnType {
                    args: rargs,
                    ret: rret,
                }) => {
                    largs.unify(rargs)?;
                    lret.unify(rret)?;
                }
                _ => return Err(CompileError::wrong_type(self, other)),
            },
            MType::Name(name) => {
                return Err(CompileError::internal(
                    format!("Encountered free type variable: {}", name).as_str(),
                ))
            }
        }

        Ok(())
    }

    fn coerce(
        op: &sqlast::BinaryOperator,
        left: &Ref<Self>,
        right: &Ref<Self>,
    ) -> Result<[Option<CRef<Self>>; 2]> {
        let df_op = match super::datafusion::parser_binop_to_df_binop(op) {
            Ok(op) => op,
            Err(e) => return Err(CompileError::unimplemented(&(e.to_string()))),
        };

        let left_type = left.read()?;
        let right_type = right.read()?;

        let left_rt = left_type.to_runtime_type()?;
        let right_rt = right_type.to_runtime_type()?;

        let left_df: ArrowDataType = (&left_rt).try_into()?;
        let right_df: ArrowDataType = (&right_rt).try_into()?;

        let coerced_df = match datafusion::logical_expr::type_coercion::binary::coerce_types(
            &left_df, &df_op, &right_df,
        ) {
            Ok(t) => t,
            Err(e) => return Err(CompileError::internal(&(e.to_string()))),
        };

        let coerced_type: Type = (&coerced_df).try_into()?;
        Ok([
            if coerced_type == left_rt {
                None
            } else {
                Some(mkcref(MType::from_runtime_type(&coerced_type)?))
            },
            if coerced_type == right_rt {
                None
            } else {
                Some(mkcref(MType::from_runtime_type(&coerced_type)?))
            },
        ])
    }
}

impl Constrainable for Vec<MField> {
    fn unify(&self, other: &Vec<MField>) -> Result<()> {
        let err = || {
            CompileError::wrong_type(&MType::Record(self.clone()), &MType::Record(other.clone()))
        };
        if self.len() != other.len() {
            return Err(err());
        }

        for i in 0..self.len() {
            if self[i].name != other[i].name {
                return Err(err());
            }

            if self[i].nullable != other[i].nullable {
                return Err(err());
            }

            self[i].type_.unify(&other[i].type_)?;
        }

        Ok(())
    }
}

impl CRef<MType> {
    pub fn substitute(&self, variables: &BTreeMap<String, CRef<MType>>) -> Result<CRef<MType>> {
        match &*self.read()? {
            Constrained::Known(t) => t.read()?.substitute(variables),
            Constrained::Unknown { .. } => Ok(self.clone()),
            Constrained::Ref(r) => r.substitute(variables),
        }
    }
}

impl<T> CRef<T>
where
    T: Constrainable + 'static,
{
    pub async fn clone_inner(&self) -> Result<T> {
        let expr = self.await?;
        let expr = expr.read()?;
        Ok(expr.clone())
    }
}

pub type Ref<T> = Arc<RwLock<T>>;

#[derive(Clone)]
pub struct SType {
    pub variables: BTreeSet<String>,
    pub body: CRef<MType>,
}

impl SType {
    pub fn new_mono(body: CRef<MType>) -> CRef<SType> {
        mkcref(SType {
            variables: BTreeSet::new(),
            body,
        })
    }

    pub fn new_poly(body: CRef<MType>, variables: BTreeSet<String>) -> CRef<SType> {
        mkcref(SType { variables, body })
    }
}

impl fmt::Debug for SType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.variables.len() > 0 {
            f.write_str("∀ ")?;
            for (i, variable) in self.variables.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                variable.fmt(f)?;
            }
            f.write_str(" ")?;
        }
        self.body.fmt(f)
    }
}

impl Constrainable for SType {}

#[derive(Clone)]
pub struct SchemaInstance {
    pub schema: SchemaRef,
    pub id: Option<usize>,
}

impl fmt::Debug for SchemaInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(f.debug_struct("FnExpr")
            .field("id", &self.id)
            .finish_non_exhaustive()?)
    }
}

impl SchemaInstance {
    pub fn global(schema: SchemaRef) -> SchemaInstance {
        SchemaInstance { schema, id: None }
    }

    pub fn instance(schema: SchemaRef, id: usize) -> SchemaInstance {
        SchemaInstance {
            schema,
            id: Some(id),
        }
    }
}

pub type Value = crate::types::Value;

pub type Params<TypeRef> = BTreeMap<ast::Ident, TypedExpr<TypeRef>>;

#[derive(Clone)]
pub struct SQLExpr<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    pub params: Params<TypeRef>,
    pub expr: sqlast::Expr,
}

impl<T: Clone + fmt::Debug + Send + Sync> fmt::Debug for SQLExpr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SQLExpr")
            .field("params", &self.params)
            .field("expr", &self.expr.to_string())
            .finish()
    }
}

#[derive(Clone)]
pub struct SQLQuery<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    pub params: Params<TypeRef>,
    pub query: sqlast::Query,
}

impl<T: Clone + fmt::Debug + Send + Sync> fmt::Debug for SQLQuery<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SQLQuery")
            .field("params", &self.params)
            .field("query", &self.query.to_string())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum FnBody<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    SQLBuiltin,
    Expr(Arc<Expr<TypeRef>>),
}

impl FnBody<CRef<MType>> {
    pub fn to_runtime_type(&self) -> runtime::error::Result<FnBody<Ref<Type>>> {
        Ok(match self {
            FnBody::SQLBuiltin => FnBody::SQLBuiltin,
            FnBody::Expr(e) => FnBody::Expr(Arc::new(e.to_runtime_type()?)),
        })
    }
}

#[derive(Clone)]
pub struct FnExpr<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    pub inner_schema: Ref<Schema>,
    pub body: FnBody<TypeRef>,
}

impl<TypeRef: Clone + fmt::Debug + Send + Sync> fmt::Debug for FnExpr<TypeRef> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(f.debug_struct("FnExpr")
            .field("body", &self.body)
            .finish_non_exhaustive()?)
    }
}

#[derive(Clone, Debug)]
pub struct FnCallExpr<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    pub func: Arc<TypedExpr<TypeRef>>,
    pub args: Vec<TypedExpr<TypeRef>>,
}

#[derive(Clone, Debug)]
pub enum Expr<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    SQLQuery(Arc<SQLQuery<TypeRef>>),
    SQLExpr(Arc<SQLExpr<TypeRef>>),
    SchemaEntry(STypedExpr),
    Fn(FnExpr<TypeRef>),
    FnCall(FnCallExpr<TypeRef>),
    NativeFn(String),
    Unknown,
}

impl Expr<CRef<MType>> {
    pub fn to_runtime_type(&self) -> runtime::error::Result<Expr<Ref<Type>>> {
        match self {
            Expr::SQLQuery(q) => {
                let SQLQuery { params, query } = q.as_ref();
                Ok(Expr::SQLQuery(Arc::new(SQLQuery {
                    params: params
                        .iter()
                        .map(|(name, param)| Ok((name.clone(), param.to_runtime_type()?)))
                        .collect::<runtime::error::Result<_>>()?,
                    query: query.clone(),
                })))
            }
            Expr::SQLExpr(e) => {
                let SQLExpr { params, expr } = e.as_ref();
                Ok(Expr::SQLExpr(Arc::new(SQLExpr {
                    params: params
                        .iter()
                        .map(|(name, param)| Ok((name.clone(), param.to_runtime_type()?)))
                        .collect::<runtime::error::Result<_>>()?,
                    expr: expr.clone(),
                })))
            }
            Expr::Fn(FnExpr { inner_schema, body }) => Ok(Expr::Fn(FnExpr {
                inner_schema: inner_schema.clone(),
                body: body.to_runtime_type()?,
            })),
            Expr::FnCall(FnCallExpr { func, args }) => Ok(Expr::FnCall(FnCallExpr {
                func: Arc::new(func.to_runtime_type()?),
                args: args
                    .iter()
                    .map(|a| Ok(a.to_runtime_type()?))
                    .collect::<runtime::error::Result<_>>()?,
            })),
            Expr::SchemaEntry(e) => Ok(Expr::SchemaEntry(e.clone())),
            Expr::NativeFn(f) => Ok(Expr::NativeFn(f.clone())),
            Expr::Unknown => Ok(Expr::Unknown),
        }
    }
}

impl<Ty: Clone + fmt::Debug + Send + Sync> Constrainable for Expr<Ty> {}

#[derive(Clone, Debug)]
pub struct TypedExpr<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    pub type_: TypeRef,
    pub expr: Arc<Expr<TypeRef>>,
}

impl TypedExpr<CRef<MType>> {
    pub fn to_runtime_type(&self) -> runtime::error::Result<TypedExpr<Ref<Type>>> {
        Ok(TypedExpr::<Ref<Type>> {
            type_: mkref(self.type_.must()?.read()?.to_runtime_type()?),
            expr: Arc::new(self.expr.to_runtime_type()?),
        })
    }
}

impl Constrainable for TypedExpr<CRef<MType>> {}

#[derive(Clone)]
pub struct STypedExpr {
    pub type_: CRef<SType>,
    pub expr: CRef<Expr<CRef<MType>>>,
}

impl STypedExpr {
    pub fn new_unknown(debug_name: &str) -> STypedExpr {
        STypedExpr {
            type_: CRef::new_unknown(&format!("{} type", debug_name)),
            expr: CRef::new_unknown(&format!("{} expr", debug_name)),
        }
    }

    pub fn to_runtime_type(&self) -> runtime::error::Result<TypedExpr<Ref<Type>>> {
        Ok(TypedExpr::<Ref<Type>> {
            type_: mkref(
                self.type_
                    .must()?
                    .read()?
                    .body
                    .must()?
                    .read()?
                    .to_runtime_type()?,
            ),
            expr: Arc::new(self.expr.must()?.read()?.to_runtime_type()?),
        })
    }
}

impl fmt::Debug for STypedExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("STypedExpr")
            .field("type_", &*self.type_.read().unwrap())
            .field("expr", &self.expr)
            .finish()
    }
}

impl Constrainable for STypedExpr {
    fn unify(&self, other: &Self) -> Result<()> {
        self.expr.unify(&other.expr)?;
        self.type_.unify(&other.type_)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum SchemaEntry {
    Schema(ast::Path),
    Type(CRef<MType>),
    Expr(STypedExpr),
}

pub fn mkref<T>(t: T) -> Ref<T> {
    Arc::new(RwLock::new(t))
}

#[derive(Clone, Debug)]
pub struct Decl {
    pub public: bool,
    pub extern_: bool,
    pub name: String,
    pub value: SchemaEntry,
}

#[derive(Clone, Debug)]
pub struct TypedNameAndExpr<TypeRef>
where
    TypeRef: Clone + fmt::Debug + Send + Sync,
{
    pub name: String,
    pub type_: TypeRef,
    pub expr: Arc<Expr<TypeRef>>,
}

pub type SchemaRef = Ref<Schema>;

#[derive(Clone, Debug)]
pub struct TypedName<TypeRef> {
    pub name: String,
    pub type_: TypeRef,
}

#[derive(Clone, Debug)]
pub struct ImportedSchema {
    pub args: Option<Vec<BTreeMap<String, TypedNameAndExpr<CRef<MType>>>>>,
    pub schema: SchemaRef,
}

// XXX We should implement a cheaper Eq / PartialEq over Schema, because it's
// currently used to check if two types are equal.
#[derive(Clone, Debug)]
pub struct Schema {
    pub folder: Option<String>,
    pub parent_scope: Option<Ref<Schema>>,
    pub externs: BTreeMap<String, CRef<MType>>,
    pub decls: BTreeMap<String, Decl>,
    pub imports: BTreeMap<ast::Path, Ref<ImportedSchema>>,
    pub exprs: Vec<CTypedExpr>,
}

impl Schema {
    pub fn new(folder: Option<String>) -> Ref<Schema> {
        mkref(Schema {
            folder,
            parent_scope: None,
            externs: BTreeMap::new(),
            decls: BTreeMap::new(),
            imports: BTreeMap::new(),
            exprs: Vec::new(),
        })
    }
}

pub const SCHEMA_EXTENSIONS: &[&str] = &["tql", "co"];
