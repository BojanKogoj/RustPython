extern crate lalrpop_util;

use std::error::Error;
use std::fs::File;
use std::io::Read;
use std::iter;
use std::path::Path;

use super::ast;
use super::lexer;
use super::python;
use super::token;

pub fn read_file(filename: &Path) -> Result<String, String> {
    match File::open(&filename) {
        Ok(mut file) => {
            let mut s = String::new();

            match file.read_to_string(&mut s) {
                Err(why) => Err(String::from("Reading file failed: ") + why.description()),
                Ok(_) => Ok(s),
            }
        }
        Err(why) => Err(String::from("Opening file failed: ") + why.description()),
    }
}

/*
 * Parse python code.
 * Grammar may be inspired by antlr grammar for python:
 * https://github.com/antlr/grammars-v4/tree/master/python3
 */

pub fn parse(filename: &Path) -> Result<ast::Program, String> {
    info!("Parsing: {}", filename.display());
    match read_file(filename) {
        Ok(txt) => {
            debug!("Read contents of file: {}", txt);
            parse_program(&txt)
        }
        Err(msg) => Err(msg),
    }
}

macro_rules! do_lalr_parsing {
    ($input: expr, $pat: ident, $tok: ident) => {{
        let lxr = lexer::make_tokenizer($input);
        let marker_token = (Default::default(), token::Tok::$tok, Default::default());
        let tokenizer = iter::once(Ok(marker_token)).chain(lxr);

        match python::TopParser::new().parse(tokenizer) {
            Err(why) => Err(format!("{:?}", why)),
            Ok(top) => {
                if let ast::Top::$pat(x) = top {
                    Ok(x)
                } else {
                    unreachable!()
                }
            }
        }
    }};
}

pub fn parse_program(source: &str) -> Result<ast::Program, String> {
    do_lalr_parsing!(source, Program, StartProgram)
}

pub fn parse_statement(source: &str) -> Result<ast::LocatedStatement, String> {
    do_lalr_parsing!(source, Statement, StartStatement)
}

pub fn parse_expression(source: &str) -> Result<ast::Expression, String> {
    do_lalr_parsing!(source, Expression, StartExpression)
}

#[cfg(test)]
mod tests {
    use super::ast;
    use super::parse_expression;
    use super::parse_program;
    use super::parse_statement;

    #[test]
    fn test_parse_empty() {
        let parse_ast = parse_program(&String::from("\n"));

        assert_eq!(parse_ast, Ok(ast::Program { statements: vec![] }))
    }

    #[test]
    fn test_parse_print_hello() {
        let source = String::from("print('Hello world')\n");
        let parse_ast = parse_program(&source).unwrap();
        assert_eq!(
            parse_ast,
            ast::Program {
                statements: vec![ast::LocatedStatement {
                    location: ast::Location::new(1, 1),
                    node: ast::Statement::Expression {
                        expression: ast::Expression::Call {
                            function: Box::new(ast::Expression::Identifier {
                                name: String::from("print"),
                            }),
                            args: vec![ast::Expression::String {
                                value: String::from("Hello world"),
                            }],
                            keywords: vec![],
                        },
                    },
                },],
            }
        );
    }

    #[test]
    fn test_parse_print_2() {
        let source = String::from("print('Hello world', 2)\n");
        let parse_ast = parse_program(&source).unwrap();
        assert_eq!(
            parse_ast,
            ast::Program {
                statements: vec![ast::LocatedStatement {
                    location: ast::Location::new(1, 1),
                    node: ast::Statement::Expression {
                        expression: ast::Expression::Call {
                            function: Box::new(ast::Expression::Identifier {
                                name: String::from("print"),
                            }),
                            args: vec![
                                ast::Expression::String {
                                    value: String::from("Hello world"),
                                },
                                ast::Expression::Number {
                                    value: ast::Number::Integer { value: 2 },
                                }
                            ],
                            keywords: vec![],
                        },
                    },
                },],
            }
        );
    }

    #[test]
    fn test_parse_kwargs() {
        let source = String::from("my_func('positional', keyword=2)\n");
        let parse_ast = parse_program(&source).unwrap();
        assert_eq!(
            parse_ast,
            ast::Program {
                statements: vec![ast::LocatedStatement {
                    location: ast::Location::new(1, 1),
                    node: ast::Statement::Expression {
                        expression: ast::Expression::Call {
                            function: Box::new(ast::Expression::Identifier {
                                name: String::from("my_func"),
                            }),
                            args: vec![ast::Expression::String {
                                value: String::from("positional"),
                            }],
                            keywords: vec![ast::Keyword {
                                name: Some("keyword".to_string()),
                                value: ast::Expression::Number {
                                    value: ast::Number::Integer { value: 2 },
                                }
                            }],
                        },
                    },
                },],
            }
        );
    }

    #[test]
    fn test_parse_if_elif_else() {
        let source = String::from("if 1: 10\nelif 2: 20\nelse: 30\n");
        let parse_ast = parse_statement(&source).unwrap();
        assert_eq!(
            parse_ast,
            ast::LocatedStatement {
                location: ast::Location::new(1, 1),
                node: ast::Statement::If {
                    test: ast::Expression::Number {
                        value: ast::Number::Integer { value: 1 },
                    },
                    body: vec![ast::LocatedStatement {
                        location: ast::Location::new(1, 7),
                        node: ast::Statement::Expression {
                            expression: ast::Expression::Number {
                                value: ast::Number::Integer { value: 10 },
                            }
                        },
                    },],
                    orelse: Some(vec![ast::LocatedStatement {
                        location: ast::Location::new(2, 1),
                        node: ast::Statement::If {
                            test: ast::Expression::Number {
                                value: ast::Number::Integer { value: 2 },
                            },
                            body: vec![ast::LocatedStatement {
                                location: ast::Location::new(2, 9),
                                node: ast::Statement::Expression {
                                    expression: ast::Expression::Number {
                                        value: ast::Number::Integer { value: 20 },
                                    },
                                },
                            },],
                            orelse: Some(vec![ast::LocatedStatement {
                                location: ast::Location::new(3, 7),
                                node: ast::Statement::Expression {
                                    expression: ast::Expression::Number {
                                        value: ast::Number::Integer { value: 30 },
                                    },
                                },
                            },]),
                        }
                    },]),
                }
            }
        );
    }

    #[test]
    fn test_parse_lambda() {
        let source = String::from("lambda x, y: x * y\n"); // lambda(x, y): x * y");
        let parse_ast = parse_statement(&source);
        assert_eq!(
            parse_ast,
            Ok(ast::LocatedStatement {
                location: ast::Location::new(1, 1),
                node: ast::Statement::Expression {
                    expression: ast::Expression::Lambda {
                        args: ast::Parameters {
                            args: vec![String::from("x"), String::from("y")],
                            kwonlyargs: vec![],
                            vararg: None,
                            kwarg: None,
                            defaults: vec![],
                            kw_defaults: vec![],
                        },
                        body: Box::new(ast::Expression::Binop {
                            a: Box::new(ast::Expression::Identifier {
                                name: String::from("x"),
                            }),
                            op: ast::Operator::Mult,
                            b: Box::new(ast::Expression::Identifier {
                                name: String::from("y"),
                            })
                        })
                    }
                }
            })
        )
    }

    #[test]
    fn test_parse_tuples() {
        let source = String::from("a, b = 4, 5\n");

        assert_eq!(
            parse_statement(&source),
            Ok(ast::LocatedStatement {
                location: ast::Location::new(1, 1),
                node: ast::Statement::Assign {
                    targets: vec![ast::Expression::Tuple {
                        elements: vec![
                            ast::Expression::Identifier {
                                name: "a".to_string()
                            },
                            ast::Expression::Identifier {
                                name: "b".to_string()
                            }
                        ]
                    }],
                    value: ast::Expression::Tuple {
                        elements: vec![
                            ast::Expression::Number {
                                value: ast::Number::Integer { value: 4 }
                            },
                            ast::Expression::Number {
                                value: ast::Number::Integer { value: 5 }
                            }
                        ]
                    }
                }
            })
        )
    }

    #[test]
    fn test_parse_class() {
        let source = String::from("class Foo(A, B):\n def __init__(self):\n  pass\n def method_with_default(self, arg='default'):\n  pass\n");
        assert_eq!(
            parse_statement(&source),
            Ok(ast::LocatedStatement {
                location: ast::Location::new(1, 1),
                node: ast::Statement::ClassDef {
                    name: String::from("Foo"),
                    bases: vec![
                        ast::Expression::Identifier {
                            name: String::from("A")
                        },
                        ast::Expression::Identifier {
                            name: String::from("B")
                        }
                    ],
                    keywords: vec![],
                    body: vec![
                        ast::LocatedStatement {
                            location: ast::Location::new(2, 2),
                            node: ast::Statement::FunctionDef {
                                name: String::from("__init__"),
                                args: ast::Parameters {
                                    args: vec![String::from("self")],
                                    kwonlyargs: vec![],
                                    vararg: None,
                                    kwarg: None,
                                    defaults: vec![],
                                    kw_defaults: vec![],
                                },
                                body: vec![ast::LocatedStatement {
                                    location: ast::Location::new(3, 3),
                                    node: ast::Statement::Pass,
                                }],
                                decorator_list: vec![],
                            }
                        },
                        ast::LocatedStatement {
                            location: ast::Location::new(4, 2),
                            node: ast::Statement::FunctionDef {
                                name: String::from("method_with_default"),
                                args: ast::Parameters {
                                    args: vec![String::from("self"), String::from("arg"),],
                                    kwonlyargs: vec![],
                                    vararg: None,
                                    kwarg: None,
                                    defaults: vec![ast::Expression::String {
                                        value: "default".to_string()
                                    }],
                                    kw_defaults: vec![],
                                },
                                body: vec![ast::LocatedStatement {
                                    location: ast::Location::new(5, 3),
                                    node: ast::Statement::Pass,
                                }],
                                decorator_list: vec![],
                            }
                        }
                    ],
                    decorator_list: vec![],
                }
            })
        )
    }

    #[test]
    fn test_parse_list_comprehension() {
        let source = String::from("[x for y in z]");
        let parse_ast = parse_expression(&source).unwrap();
        assert_eq!(
            parse_ast,
            ast::Expression::Comprehension {
                kind: Box::new(ast::ComprehensionKind::List {
                    element: ast::Expression::Identifier {
                        name: "x".to_string()
                    }
                }),
                generators: vec![ast::Comprehension {
                    target: ast::Expression::Identifier {
                        name: "y".to_string()
                    },
                    iter: ast::Expression::Identifier {
                        name: "z".to_string()
                    },
                    ifs: vec![],
                }],
            }
        );
    }

    #[test]
    fn test_parse_double_list_comprehension() {
        let source = String::from("[x for y, y2 in z for a in b if a < 5 if a > 10]");
        let parse_ast = parse_expression(&source).unwrap();
        assert_eq!(
            parse_ast,
            ast::Expression::Comprehension {
                kind: Box::new(ast::ComprehensionKind::List {
                    element: ast::Expression::Identifier {
                        name: "x".to_string()
                    }
                }),
                generators: vec![
                    ast::Comprehension {
                        target: ast::Expression::Tuple {
                            elements: vec![
                                ast::Expression::Identifier {
                                    name: "y".to_string()
                                },
                                ast::Expression::Identifier {
                                    name: "y2".to_string()
                                },
                            ],
                        },
                        iter: ast::Expression::Identifier {
                            name: "z".to_string()
                        },
                        ifs: vec![],
                    },
                    ast::Comprehension {
                        target: ast::Expression::Identifier {
                            name: "a".to_string()
                        },
                        iter: ast::Expression::Identifier {
                            name: "b".to_string()
                        },
                        ifs: vec![
                            ast::Expression::Compare {
                                a: Box::new(ast::Expression::Identifier {
                                    name: "a".to_string()
                                }),
                                op: ast::Comparison::Less,
                                b: Box::new(ast::Expression::Number {
                                    value: ast::Number::Integer { value: 5 }
                                }),
                            },
                            ast::Expression::Compare {
                                a: Box::new(ast::Expression::Identifier {
                                    name: "a".to_string()
                                }),
                                op: ast::Comparison::Greater,
                                b: Box::new(ast::Expression::Number {
                                    value: ast::Number::Integer { value: 10 }
                                }),
                            },
                        ],
                    }
                ],
            }
        );
    }
}
