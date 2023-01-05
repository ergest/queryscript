use snafu::prelude::*;

use crate::ast;
use crate::ast::ToIdents;
use crate::compile;
use crate::compile::schema;
use crate::parser;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::read_dir;
use std::path::Path;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub struct AutoCompleterStats {
    pub tried: u64,
    pub completed: u64,
    pub msg: String,
}

impl AutoCompleterStats {
    pub fn new() -> AutoCompleterStats {
        AutoCompleterStats {
            tried: 0,
            completed: 0,
            msg: String::new(),
        }
    }
}

impl fmt::Display for AutoCompleterStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{:?}", self))?;
        Ok(())
    }
}

pub struct AutoCompleter {
    pub compiler: compile::Compiler,
    pub schema: schema::Ref<schema::Schema>,
    pub curr_buffer: Rc<RefCell<String>>,
    pub stats: Rc<RefCell<AutoCompleterStats>>,
    pub debug: bool,
}

impl AutoCompleter {
    pub fn new(
        compiler: compile::Compiler,
        schema: schema::Ref<schema::Schema>,
        curr_buffer: Rc<RefCell<String>>,
    ) -> AutoCompleter {
        AutoCompleter {
            compiler,
            schema,
            curr_buffer,
            stats: Rc::new(RefCell::new(AutoCompleterStats::new())),
            debug: false, // Switch this to true to get diagnostics as you type
        }
    }
}

pub fn pos_to_loc(text: &str, pos: usize) -> parser::Location {
    let line: u64 = (text[..pos]
        .as_bytes()
        .iter()
        .filter(|&&c| c == b'\n')
        .count()
        + 1) as u64;
    let column: u64 = match text[..pos].rfind('\n') {
        Some(nl) => pos - nl,
        None => pos + 1,
    } as u64;
    parser::Location { line, column }
}

pub fn loc_to_pos(text: &str, loc: parser::Location) -> usize {
    text.split('\n').collect::<Vec<_>>()[..(loc.line - 1) as usize]
        .iter()
        .map(|l| l.len() + 1)
        .sum::<usize>()
        + loc.column as usize
        - 1
}

fn parse_longest_path(texts: &Vec<ast::Ident>) -> Vec<ast::Located<ast::Ident>> {
    texts
        .iter()
        .fold::<Vec<ast::Located<ast::Ident>>, _>(Vec::new(), |acc, item| {
            let parsed = if item.as_str().is_empty() {
                Vec::new()
            } else {
                match parser::parse_path("<repl>", item.as_str()) {
                    Ok(path) => path,
                    Err(_) => Vec::new(),
                }
            };
            if acc.len() < parsed.len() {
                parsed
            } else {
                acc
            }
        })
}

fn get_imported_decls<F: FnMut(&schema::SchemaEntry) -> bool>(
    compiler: compile::Compiler,
    schema: schema::Ref<schema::Schema>,
    path: &Vec<ast::Located<ast::Ident>>,
    mut f: F,
) -> compile::Result<Vec<ast::Ident>> {
    let (schema, _, remainder) = compile::lookup_path(compiler, schema.clone(), path, true, true)?;
    if remainder.len() > 0 {
        return Ok(Vec::new());
    }
    return Ok(schema
        .read()?
        .decls
        .iter()
        .filter_map(move |(k, v)| if f(&v.value) { Some(k.clone()) } else { None })
        .collect::<Vec<ast::Ident>>());
}

fn get_schema_paths(
    schema: schema::Ref<schema::Schema>,
    path: &Vec<ast::Ident>,
) -> compile::Result<Vec<String>> {
    if let Some(folder) = schema.read()?.folder.clone() {
        let mut folder = Path::new(&folder).to_path_buf();
        folder.extend(path.iter().map(|s| s.to_string()));
        let files = read_dir(folder)?;
        let mut ret = Vec::new();
        for f in files {
            if let Ok(f) = f {
                let file = f.path();
                let extension = file.extension().and_then(OsStr::to_str).unwrap_or("");
                if schema::SCHEMA_EXTENSIONS.contains(&extension) {
                    if let Some(fp) = file.file_stem().and_then(OsStr::to_str) {
                        ret.push(fp.to_string());
                    }
                }
                if file.is_dir() {
                    if let Some(fp) = file.file_name().and_then(OsStr::to_str) {
                        ret.push(fp.to_string());
                    }
                }
            }
        }
        return Ok(ret);
    }
    Ok(Vec::new())
}

fn get_record_fields(
    compiler: compile::Compiler,
    schema: schema::Ref<schema::Schema>,
    path: &ast::Path,
) -> compile::Result<Vec<ast::Ident>> {
    let expr = compile::compile_reference(compiler.clone(), schema.clone(), path)?;
    let type_ = expr
        .type_
        .must()
        .context(compile::error::RuntimeSnafu {
            loc: compile::error::ErrorLocation::Unknown,
        })?
        .read()?
        .clone();

    match type_ {
        schema::MType::Record(fields) => {
            return Ok(fields.iter().map(|f| f.name.clone()).collect());
        }
        _ => {}
    }

    Ok(Vec::new())
}

impl AutoCompleter {
    pub fn auto_complete(&self, line: &str, pos: usize) -> (usize, Vec<ast::Ident>) {
        (&mut *self.stats.borrow_mut()).tried += 1;

        let mut full = self.curr_buffer.borrow().clone();
        let start_pos = full.len();
        full.push_str(&line);

        let full_pos = start_pos + pos;
        let full_loc = pos_to_loc(full.as_str(), full_pos);

        let (tokens, eof) = match parser::tokenize("<repl>", &full) {
            Ok(r) => r,
            Err(e) => {
                (&mut *self.stats.borrow_mut()).msg = format!("{}", e);
                return (0, Vec::new());
            }
        };
        let parser = parser::Parser::new("<repl>", tokens, eof);

        let (tok, suggestions) = parser.get_autocomplete(full_loc);
        let partial = match tok.token {
            parser::Token::Word(w) => w.value,
            _ => "".to_string(),
        };
        let suggestion_loc = tok.location.clone();
        let suggestion_pos = loc_to_pos(full.as_str(), suggestion_loc);

        if suggestion_pos < start_pos {
            (&mut *self.stats.borrow_mut()).msg = format!("failed before");
            return (0, Vec::new());
        }

        let mut ident_types = BTreeMap::<char, Vec<ast::Ident>>::new();
        for s in suggestions.clone() {
            match s {
                parser::Token::Word(w) => {
                    let style = match w.quote_style {
                        None | Some(parser::AUTOCOMPLETE_KEYWORD) => parser::AUTOCOMPLETE_KEYWORD,
                        Some('\"') | Some(parser::AUTOCOMPLETE_VARIABLE) => {
                            parser::AUTOCOMPLETE_VARIABLE
                        }
                        Some(c) => c,
                    };
                    ident_types
                        .entry(style)
                        .or_insert_with(Vec::new)
                        .push(w.into());
                }
                _ => {}
            }
        }

        let vars = ident_types
            .get(&parser::AUTOCOMPLETE_VARIABLE)
            .map(parse_longest_path)
            .map_or(Vec::new(), |path| {
                if let Ok(choices) =
                    get_imported_decls(self.compiler.clone(), self.schema.clone(), &path, |se| {
                        matches!(se, schema::SchemaEntry::Expr(_))
                    })
                {
                    return choices;
                }
                if let Ok(choices) =
                    get_record_fields(self.compiler.clone(), self.schema.clone(), &path)
                {
                    return choices;
                }
                Vec::new()
            })
            .clone();

        let types = ident_types
            .get(&parser::AUTOCOMPLETE_TYPE)
            .map(parse_longest_path)
            .map_or(Vec::new(), |path| {
                if let Ok(choices) =
                    get_imported_decls(self.compiler.clone(), self.schema.clone(), &path, |se| {
                        matches!(se, schema::SchemaEntry::Type(_))
                            || matches!(se, schema::SchemaEntry::Schema(_))
                    })
                {
                    return choices;
                }
                Vec::new()
            });

        let schemas = ident_types
            .get(&parser::AUTOCOMPLETE_SCHEMA)
            .map(parse_longest_path)
            .map_or(Vec::new(), |path| {
                if let Ok(choices) = get_schema_paths(self.schema.clone(), &path.to_idents()) {
                    return choices.into_iter().map(|s| s.into()).collect();
                }
                Vec::new()
            });

        let mut keywords = ident_types
            .get(&parser::AUTOCOMPLETE_KEYWORD)
            .unwrap_or(&Vec::new())
            .clone();

        if match partial.chars().next() {
            Some(c) => c.is_lowercase(),
            None => false,
        } {
            keywords = keywords.iter().map(|k| k.to_lowercase()).collect();
        }

        let all = vec![vars, types, schemas, keywords].concat();
        let filtered = all
            .into_iter()
            // TODO: This may need to be implemented to be case insensitive
            .filter(|a| a.as_str().starts_with(partial.as_str()))
            .collect::<Vec<_>>();

        (&mut *self.stats.borrow_mut()).msg = format!(
            "{} {:?} {:?}",
            suggestion_pos - start_pos,
            partial,
            filtered,
        );

        (&mut *self.stats.borrow_mut()).completed += 1;

        (suggestion_pos - start_pos, filtered)
    }
}
