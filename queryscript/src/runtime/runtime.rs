use futures::future::{BoxFuture, FutureExt};
use std::collections::HashMap;

use crate::compile::schema;
use crate::{
    ast::Ident,
    types,
    types::{Arc, Value},
};

use super::{context::Context, error::*, sql::SQLParam};

type TypeRef = schema::Ref<types::Type>;

// This is a type alias for simplicity and to make it easy potentially in the future to allow a
// library user to pass in their own runtime.
pub type Runtime = tokio::runtime::Runtime;

pub fn build() -> Result<Runtime> {
    Ok(if cfg!(feature = "multi-thread") {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    }
    .build()?)
}

pub async fn eval_params<'a>(
    ctx: &'a Context,
    params: &'a schema::Params<TypeRef>,
) -> Result<HashMap<Ident, SQLParam>> {
    let mut param_values = HashMap::new();
    for (name, param) in params {
        let value = eval(ctx, param).await?;
        param_values.insert(
            name.clone(),
            SQLParam::new(name.clone(), value, &*param.type_.read()?),
        );
    }

    Ok(param_values)
}

pub fn eval<'a>(
    ctx: &'a Context,
    typed_expr: &'a schema::TypedExpr<TypeRef>,
) -> BoxFuture<'a, crate::runtime::Result<crate::types::Value>> {
    async move {
        match &*typed_expr.expr.as_ref() {
            schema::Expr::Unknown => {
                return Err(RuntimeError::new("unresolved extern"));
            }
            schema::Expr::SchemaEntry(schema::STypedExpr { expr, .. }) => {
                let rt_expr = {
                    let expr = expr.must()?;
                    let expr = expr.read()?;
                    Arc::new(expr.to_runtime_type()?)
                };

                eval(
                    ctx,
                    &schema::TypedExpr {
                        type_: typed_expr.type_.clone(),
                        expr: rt_expr,
                    },
                )
                .await
            }
            schema::Expr::ContextRef(r) => match ctx.values.get(r) {
                Some(v) => Ok(v.clone()), // Can we avoid this clone??
                None => Err(RuntimeError::new(
                    format!("No such context value {}", r).as_str(),
                )),
            },
            schema::Expr::Fn(f) => {
                use super::functions::*;
                let body = match &f.body {
                    schema::FnBody::Expr(e) => e.clone(),
                    _ => {
                        return fail!(
                            "Non-expression function body should have been optimized away"
                        )
                    }
                };
                QSFn::new(typed_expr.type_.clone(), body)
            }
            schema::Expr::NativeFn(name) => {
                use super::functions::*;
                match name.as_str() {
                    "load" => Ok(Value::Fn(Arc::new(LoadFileFn::new(
                        &*typed_expr.type_.read()?,
                    )?))),
                    "__native_identity" => Ok(Value::Fn(Arc::new(IdentityFn::new(
                        &*typed_expr.type_.read()?,
                    )?))),
                    _ => return rt_unimplemented!("native function: {}", name),
                }
            }
            schema::Expr::FnCall(schema::FnCallExpr {
                func,
                args,
                ctx_folder,
            }) => {
                let mut new_ctx = ctx.clone();
                new_ctx.folder = ctx_folder.clone();
                let mut arg_values = Vec::new();
                for arg in args.iter() {
                    // Eval the arguments in the calling context
                    //
                    arg_values.push(eval(ctx, arg).await?);
                }
                let fn_val = match eval(&new_ctx, func.as_ref()).await? {
                    Value::Fn(f) => f,
                    _ => return fail!("Cannot call non-function"),
                };

                fn_val.execute(&new_ctx, arg_values).await
            }
            schema::Expr::SQL(e) => {
                let schema::SQL { body, names } = e.as_ref();
                let sql_params = eval_params(ctx, &names.params).await?;
                let query = body.as_query();

                // TODO: This ownership model implies some necessary copying (below).
                let rows = { ctx.sql_engine.eval(ctx, &query, sql_params).await? };

                // Before returning, we perform some runtime checks that might only be necessary in debug mode:
                // - For expressions, validate that the result is a single row and column
                // - For expressions and queries, check that the RecordBatch's type matches the
                //   expected type from the compiler.
                let expected_type = typed_expr.type_.read()?;
                match body {
                    schema::SQLBody::Expr(_) => {
                        if rows.num_batches() != 1 {
                            return fail!("Expected an expression to have exactly one row");
                        }
                        if rows.schema().len() != 1 {
                            return fail!("Expected an expression to have exactly one column");
                        }

                        let row = &rows.batch(0).records()[0];
                        let value = row.column(0).clone();
                        let value_type = value.type_();
                        if !ctx.disable_typechecks && *expected_type != value_type {
                            return Err(RuntimeError::type_mismatch(
                                expected_type.clone(),
                                value_type,
                            ));
                        }
                        Ok(row.column(0).clone())
                    }
                    schema::SQLBody::Query(_) | schema::SQLBody::Table(_) => {
                        // Validate that the schema matches the expected type. If not, we have a serious problem
                        // since we may interpret the record batch as a different type than expected.
                        if !ctx.disable_typechecks && rows.num_batches() > 0 {
                            let rows_type = crate::types::Type::List(Box::new(
                                crate::types::Type::Record(rows.schema()),
                            ));
                            if *expected_type != rows_type {
                                return Err(RuntimeError::type_mismatch(
                                    expected_type.clone(),
                                    rows_type,
                                ));
                            }
                        }

                        Ok(Value::Relation(rows))
                    }
                }
            }
        }
    }
    .boxed()
}
