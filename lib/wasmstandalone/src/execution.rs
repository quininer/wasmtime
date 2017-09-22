use cretonne::Context;
use cretonne::settings;
use cretonne::isa::{self, TargetIsa};
use cretonne::verify_function;
use cretonne::verifier;
use cretonne::settings::Configurable;
use cretonne::result::CtonError;
use cretonne::ir::entities::AnyEntity;
use cretonne::ir::{Ebb, FuncRef, JumpTable, Function};
use cretonne::binemit::{RelocSink, Reloc, CodeOffset};
use cton_wasm::{TranslationResult, FunctionIndex, WasmRuntime};
use std::mem::transmute;
use region::Protection;
use region::protect;
use std::collections::HashMap;
use std::ptr::write_unaligned;
use std::fmt::Write;
use standalone::StandaloneRuntime;

type RelocRef = u16;

// Implementation of a relocation sink that just saves all the information for later
struct StandaloneRelocSink {
    ebbs: HashMap<RelocRef, (Ebb, CodeOffset)>,
    funcs: HashMap<RelocRef, (FuncRef, CodeOffset)>,
    jts: HashMap<RelocRef, (JumpTable, CodeOffset)>,
}

// Contains all the metadata necessary to perform relocations
struct FunctionMetaData {
    relocs: StandaloneRelocSink,
    il_func: Function,
}

impl RelocSink for StandaloneRelocSink {
    fn reloc_ebb(&mut self, offset: CodeOffset, reloc: Reloc, ebb: Ebb) {
        self.ebbs.insert(reloc.0, (ebb, offset));
    }
    fn reloc_func(&mut self, offset: CodeOffset, reloc: Reloc, func: FuncRef) {
        self.funcs.insert(reloc.0, (func, offset));
    }
    fn reloc_jt(&mut self, offset: CodeOffset, reloc: Reloc, jt: JumpTable) {
        self.jts.insert(reloc.0, (jt, offset));
    }
}

impl StandaloneRelocSink {
    fn new() -> StandaloneRelocSink {
        StandaloneRelocSink {
            ebbs: HashMap::new(),
            funcs: HashMap::new(),
            jts: HashMap::new(),
        }
    }
}

/// Structure containing the compiled code of the functions, ready to be executed.
pub struct ExecutableCode {
    functions_code: Vec<Vec<u8>>,
    start_index: FunctionIndex,
}

/// Executes a module that has been translated with the `StandaloneRuntime` runtime implementation.
pub fn compile_module(
    trans_result: &TranslationResult,
    isa: &TargetIsa,
    runtime: &StandaloneRuntime,
) -> Result<ExecutableCode, String> {
    debug_assert!(
        trans_result.start_index.is_none() ||
            trans_result.start_index.unwrap() >= trans_result.function_imports_count,
        "imported start functions not supported yet"
    );

    let mut shared_builder = settings::builder();
    shared_builder.enable("enable_verifier").expect(
        "Missing enable_verifier setting",
    );
    shared_builder.set("is_64bit", "1").expect(
        "Missing 64bits setting",
    );
    let mut functions_metatada = Vec::new();
    let mut functions_code = Vec::new();
    for (function_index, function) in trans_result.functions.iter().enumerate() {
        let mut context = Context::new();
        verify_function(function, isa).unwrap();
        context.func = function.clone(); // TODO: Avoid this clone.
        let code_size = context.compile(isa).map_err(|e| {
            pretty_error(&context.func, Some(isa), e)
        })? as usize;
        if code_size == 0 {
            return Err(String::from("no code generated by Cretonne"));
        }
        let mut code_buf: Vec<u8> = Vec::with_capacity(code_size);
        code_buf.resize(code_size, 0);
        let mut relocsink = StandaloneRelocSink::new();
        context.emit_to_memory(code_buf.as_mut_ptr(), &mut relocsink, isa);
        functions_metatada.push(FunctionMetaData {
            relocs: relocsink,
            il_func: context.func,
        });
        functions_code.push(code_buf);
    }
    relocate(
        trans_result.function_imports_count,
        &functions_metatada,
        &mut functions_code,
        runtime,
    );
    // After having emmitted the code to memory, we deal with relocations
    match trans_result.start_index {
        None => Err(String::from(
            "No start function defined, aborting execution",
        )),
        Some(index) => {
            Ok(ExecutableCode {
                functions_code,
                start_index: index,
            })
        }
    }
}

/// Jumps to the code region of memory and execute the start function of the module.
pub fn execute(exec: &ExecutableCode) -> Result<(), String> {
    let code_buf = &exec.functions_code[exec.start_index];
    unsafe {
        match protect(
            code_buf.as_ptr(),
            code_buf.len(),
            Protection::ReadWriteExecute,
        ) {
            Ok(()) => (),
            Err(err) => {
                return Err(format!(
                    "failed to give executable permission to code: {}",
                    err.description()
                ))
            }
        };
        // Rather than writing inline assembly to jump to the code region, we use the fact that
        // the Rust ABI for calling a function with no arguments and no return matches the one of
        // the generated code.Thanks to this, we can transmute the code region into a first-class
        // Rust function and call it.
        let start_func = transmute::<_, fn()>(code_buf.as_ptr());
        start_func();
        Ok(())
    }
}

/// Performs the relocations inside the function bytecode, provided the necessary metadata
fn relocate(
    function_imports_count: usize,
    functions_metatada: &[FunctionMetaData],
    functions_code: &mut Vec<Vec<u8>>,
    runtime: &StandaloneRuntime,
) {
    // The relocations are relative to the relocation's address plus four bytes
    for (func_index, function_in_memory) in functions_metatada.iter().enumerate() {
        let FunctionMetaData {
            ref relocs,
            ref il_func,
        } = *function_in_memory;
        for &(func_ref, offset) in relocs.funcs.values() {
            let target_func_index = runtime.func_indices[func_ref] - function_imports_count;
            let target_func_address: isize = functions_code[target_func_index].as_ptr() as isize;
            unsafe {
                let reloc_address: isize = functions_code[func_index].as_mut_ptr().offset(
                    offset as isize +
                        4,
                ) as isize;
                let reloc_delta_i32: i32 = (target_func_address - reloc_address) as i32;
                write_unaligned(reloc_address as *mut i32, reloc_delta_i32);
            }
        }
        for &(ebb, offset) in relocs.ebbs.values() {
            unsafe {
                let reloc_address: isize = functions_code[func_index].as_mut_ptr().offset(
                    offset as isize +
                        4,
                ) as isize;
                let target_ebb_address: isize = functions_code[func_index].as_ptr().offset(
                    il_func.offsets[ebb] as
                        isize,
                ) as isize;
                let reloc_delta_i32: i32 = (target_ebb_address - reloc_address) as i32;
                write_unaligned(reloc_address as *mut i32, reloc_delta_i32);
            }
        }
        // TODO: deal with jumptable relocations
    }
}

/// Pretty-print a verifier error.
pub fn pretty_verifier_error(
    func: &Function,
    isa: Option<&TargetIsa>,
    err: &verifier::Error,
) -> String {
    let mut msg = err.to_string();
    match err.location {
        AnyEntity::Inst(inst) => {
            write!(msg, "\n{}: {}\n\n", inst, func.dfg.display_inst(inst, isa)).unwrap()
        }
        _ => msg.push('\n'),
    }
    write!(msg, "{}", func.display(isa)).unwrap();
    msg
}

/// Pretty-print a Cretonne error.
pub fn pretty_error(func: &Function, isa: Option<&TargetIsa>, err: CtonError) -> String {
    if let CtonError::Verifier(e) = err {
        pretty_verifier_error(func, isa, &e)
    } else {
        err.to_string()
    }
}
