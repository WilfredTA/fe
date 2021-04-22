#![feature(external_doc)]
#![doc(include = "../README.md")]

use std::fs;
use std::io::{
    Error,
    Write,
};
use std::path::Path;

use clap::{
    arg_enum,
    values_t,
    App,
    Arg,
};

mod _utils;
use crate::_utils::pretty_curly_print;
use fe_common::diagnostics::print_diagnostics;
use fe_common::files::FileStore;
use fe_compiler::errors::{
    install_compiler_panic_hook,
    ErrorKind,
};
use fe_compiler::types::CompiledModule;

const DEFAULT_OUTPUT_DIR_NAME: &str = "output";
const VERSION: &str = env!("CARGO_PKG_VERSION");

arg_enum! {
    #[derive(PartialEq, Debug)]
    pub enum CompilationTarget {
        Abi,
        Ast,
        LoweredAst,
        Bytecode,
        Tokens,
        Yul,
    }
}

pub fn main() {
    install_compiler_panic_hook();

    let matches = App::new("Fe")
        .version(VERSION)
        .about("Compiler for the Fe language")
        .arg(
            Arg::with_name("input")
                .help("The input source file to use e.g erc20.fe")
                .index(1)
                .required(true),
        )
        .arg(
            Arg::with_name("output-dir")
                .short("o")
                .long("output-dir")
                .help("The directory to store the compiler output e.g /tmp/output")
                .takes_value(true)
                .default_value(DEFAULT_OUTPUT_DIR_NAME),
        )
        .arg(
            Arg::with_name("emit")
                .short("e")
                .long("emit")
                .help("Comma separated compile targets e.g. -e=bytecode,yul")
                .possible_values(&["abi", "bytecode", "ast", "tokens", "yul", "loweredAst"])
                .default_value("abi,bytecode")
                .use_delimiter(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("overwrite")
                .long("overwrite")
                .help("Overwrite contents of output directory`"),
        )
        .arg(
            Arg::with_name("optimize")
                .long("optimize")
                .help("Enables the Yul optimizer`"),
        )
        .get_matches();

    let input_file = matches.value_of("input").unwrap();
    let output_dir = matches.value_of("output-dir").unwrap();
    let overwrite = matches.is_present("overwrite");
    let optimize = matches.is_present("optimize");
    let targets =
        values_t!(matches.values_of("emit"), CompilationTarget).unwrap_or_else(|e| e.exit());
    let with_bytecode = targets.contains(&CompilationTarget::Bytecode);
    #[cfg(not(feature = "solc-backend"))]
    if with_bytecode {
        eprintln!("Warning: bytecode output requires 'solc-backend' feature. Try `cargo build --release --features solc-backend`. Skipping.");
    }

    let mut files = FileStore::new();
    let file = files
        .load_file(input_file.to_string())
        .map_err(ioerr_to_string);
    if let Err(err) = file {
        eprintln!("Failed to load file: `{}`. Error: {}", input_file, err);
        std::process::exit(1);
    }
    let (content, id) = file.unwrap();

    let compiled_module = match fe_compiler::compile(&content, id, with_bytecode, optimize) {
        Ok(module) => module,
        Err(error) => {
            eprintln!("Unable to compile {}.", input_file);
            for err in error.errors {
                match err {
                    ErrorKind::Str(err) => eprintln!("Compiler error: {}", err),
                    ErrorKind::Analyzer(err) => {
                        eprintln!("Analyzer error: {}", err.format_user(&content))
                    }
                    ErrorKind::Parser(diags) => print_diagnostics(&diags, &files),
                }
            }
            std::process::exit(1)
        }
    };
    match write_compiled_module(compiled_module, &content, &targets, &output_dir, overwrite) {
        Ok(_) => println!("Compiled {}. Outputs in `{}`", input_file, output_dir),
        Err(err) => {
            eprintln!(
                "Failed to write output to directory: `{}`. Error: {}",
                output_dir, err
            );
            std::process::exit(1)
        }
    }
}

fn write_compiled_module(
    mut module: CompiledModule,
    file_content: &str,
    targets: &[CompilationTarget],
    output_dir: &str,
    overwrite: bool,
) -> Result<(), String> {
    let output_dir = Path::new(output_dir);
    if output_dir.is_file() {
        return Err(format!(
            "A file exists at path `{}`, the location of the output directory. Refusing to overwrite.",
            output_dir.display()
        ));
    }

    if !overwrite {
        verify_nonexistent_or_empty(output_dir)?;
    }

    fs::create_dir_all(output_dir).map_err(ioerr_to_string)?;

    if targets.contains(&CompilationTarget::Ast) {
        write_output(&output_dir.join("module.ast"), &module.src_ast)?;
    }

    if targets.contains(&CompilationTarget::LoweredAst) {
        write_output(&output_dir.join("lowered_module.ast"), &module.lowered_ast)?;
    }

    if targets.contains(&CompilationTarget::Tokens) {
        let tokens = {
            let lexer = fe_parser::lexer::Lexer::new(file_content);
            lexer.collect::<Vec<_>>()
        };
        write_output(&output_dir.join("module.tokens"), &format!("{:#?}", tokens))?;
    }

    for (name, contract) in module.contracts.drain() {
        let contract_output_dir = output_dir.join(&name);
        fs::create_dir_all(&contract_output_dir).map_err(ioerr_to_string)?;

        if targets.contains(&CompilationTarget::Abi) {
            let file_name = format!("{}_abi.json", &name);
            write_output(&contract_output_dir.join(file_name), &contract.json_abi)?;
        }

        if targets.contains(&CompilationTarget::Yul) {
            let file_name = format!("{}_ir.yul", &name);
            write_output(
                &contract_output_dir.join(file_name),
                &pretty_curly_print(&contract.yul, 4),
            )?;
        }

        #[cfg(feature = "solc-backend")]
        if targets.contains(&CompilationTarget::Bytecode) {
            let file_name = format!("{}.bin", &name);
            write_output(&contract_output_dir.join(file_name), &contract.bytecode)?;
        }
    }

    Ok(())
}

fn write_output(path: &Path, content: &str) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(ioerr_to_string)?;
    file.write_all(content.as_bytes())
        .map_err(ioerr_to_string)?;
    Ok(())
}

fn ioerr_to_string(error: Error) -> String {
    format!("{}", error)
}

fn verify_nonexistent_or_empty(dir: &Path) -> Result<(), String> {
    if !dir.exists() || dir.read_dir().map_err(ioerr_to_string)?.next().is_none() {
        Ok(())
    } else {
        Err(format!(
            "Directory '{}' is not empty. Use --overwrite to overwrite.",
            dir.display()
        ))
    }
}
