extern crate rustpython_parser;

use self::rustpython_parser::ast;
use std::collections::hash_map::HashMap;
use std::fmt;
use std::path::PathBuf;

use super::builtins;
use super::bytecode;
use super::import::import;
use super::obj::objbool;
use super::obj::objiter;
use super::obj::objlist;
use super::obj::objstr;
use super::obj::objtuple;
use super::obj::objtype;
use super::pyobject::{
    AttributeProtocol, DictProtocol, IdProtocol, ParentProtocol, PyFuncArgs, PyObject,
    PyObjectKind, PyObjectRef, PyResult, ToRust, TypeProtocol,
};
use super::vm::VirtualMachine;

#[derive(Clone, Debug)]
enum Block {
    Loop {
        start: bytecode::Label,
        end: bytecode::Label,
    },
    TryExcept {
        handler: bytecode::Label,
    },
    With {
        end: bytecode::Label,
        context_manager: PyObjectRef,
    },
}

pub struct Frame {
    // TODO: We are using Option<i32> in stack for handline None return value
    pub code: bytecode::CodeObject,
    // We need 1 stack per frame
    stack: Vec<PyObjectRef>, // The main data frame of the stack machine
    blocks: Vec<Block>,      // Block frames, for controling loops and exceptions
    pub locals: PyObjectRef, // Variables
    pub lasti: usize,        // index of last instruction ran
                             // cmp_op: Vec<&'a Fn(NativeType, NativeType) -> bool>, // TODO: change compare to a function list
}

pub fn copy_code(code_obj: PyObjectRef) -> bytecode::CodeObject {
    let code_obj = code_obj.borrow();
    if let PyObjectKind::Code { ref code } = code_obj.kind {
        code.clone()
    } else {
        panic!("Must be code obj");
    }
}

// Running a frame can result in one of the below:
pub enum ExecutionResult {
    Return(PyObjectRef),
    Yield(PyObjectRef),
}

// A valid execution result, or an exception
pub type FrameResult = Result<ExecutionResult, PyObjectRef>;

impl Frame {
    pub fn new(code: PyObjectRef, globals: PyObjectRef) -> Frame {
        //populate the globals and locals
        //TODO: This is wrong, check https://github.com/nedbat/byterun/blob/31e6c4a8212c35b5157919abff43a7daa0f377c6/byterun/pyvm2.py#L95
        /*
        let globals = match globals {
            Some(g) => g,
            None => HashMap::new(),
        };
        */
        let locals = globals;
        // locals.extend(callargs);

        Frame {
            code: copy_code(code),
            stack: vec![],
            blocks: vec![],
            // save the callargs as locals
            // globals: locals.clone(),
            locals: locals,
            lasti: 0,
        }
    }

    pub fn run_frame_full(&mut self, vm: &mut VirtualMachine) -> PyResult {
        match self.run_frame(vm) {
            Ok(ExecutionResult::Return(value)) => Ok(value),
            Err(err) => Err(err),
            _ => panic!("Got unexpected result from function"),
        }
    }

    pub fn run_frame(&mut self, vm: &mut VirtualMachine) -> FrameResult {
        let filename = if let Some(source_path) = &self.code.source_path {
            source_path.to_string()
        } else {
            "<unknown>".to_string()
        };

        // This is the name of the object being run:
        let run_obj_name = &self.code.obj_name.to_string();

        // Execute until return or exception:
        let value = loop {
            let lineno = self.get_lineno();
            let result = self.execute_instruction(vm);
            match result {
                None => {}
                Some(Ok(value)) => {
                    break Ok(value);
                }
                Some(Err(exception)) => {
                    // unwind block stack on exception and find any handlers.
                    // Add an entry in the traceback:
                    assert!(objtype::isinstance(
                        &exception,
                        vm.ctx.exceptions.base_exception_type.clone()
                    ));
                    let traceback = vm
                        .get_attribute(exception.clone(), &"__traceback__".to_string())
                        .unwrap();
                    trace!("Adding to traceback: {:?} {:?}", traceback, lineno);
                    let pos = vm.ctx.new_tuple(vec![
                        vm.ctx.new_str(filename.clone()),
                        vm.ctx.new_int(lineno.get_row() as i32),
                        vm.ctx.new_str(run_obj_name.clone()),
                    ]);
                    objlist::list_append(
                        vm,
                        PyFuncArgs {
                            args: vec![traceback, pos],
                            kwargs: vec![],
                        },
                    )
                    .unwrap();
                    // exception.__trace
                    match self.unwind_exception(vm, exception) {
                        None => {}
                        Some(exception) => {
                            // TODO: append line number to traceback?
                            // traceback.append();
                            break Err(exception);
                        }
                    }
                }
            }
        };

        value
    }

    pub fn fetch_instruction(&mut self) -> bytecode::Instruction {
        // TODO: an immutable reference is enough, we should not
        // clone the instruction.
        let ins2 = self.code.instructions[self.lasti].clone();
        self.lasti += 1;
        ins2
    }

    // Execute a single instruction:
    fn execute_instruction(&mut self, vm: &mut VirtualMachine) -> Option<FrameResult> {
        let instruction = self.fetch_instruction();
        {
            trace!("=======");
            /* TODO:
            for frame in self.frames.iter() {
                trace!("  {:?}", frame);
            }
            */
            trace!("  {:?}", self);
            trace!("  Executing op code: {:?}", instruction);
            trace!("=======");
        }

        match &instruction {
            bytecode::Instruction::LoadConst { ref value } => {
                let obj = self.unwrap_constant(vm, value);
                self.push_value(obj);
                None
            }
            bytecode::Instruction::Import {
                ref name,
                ref symbol,
            } => self.import(vm, name, symbol),
            bytecode::Instruction::LoadName { ref name } => self.load_name(vm, name),
            bytecode::Instruction::StoreName { ref name } => self.store_name(name),
            bytecode::Instruction::DeleteName { ref name } => self.delete_name(vm, name),
            bytecode::Instruction::StoreSubscript => self.execute_store_subscript(vm),
            bytecode::Instruction::DeleteSubscript => self.execute_delete_subscript(vm),
            bytecode::Instruction::Pop => {
                // Pop value from stack and ignore.
                self.pop_value();
                None
            }
            bytecode::Instruction::Duplicate => {
                // Duplicate top of stack
                let value = self.pop_value();
                self.push_value(value.clone());
                self.push_value(value);
                None
            }
            bytecode::Instruction::Rotate { amount } => {
                // Shuffles top of stack amount down
                if amount < &2 {
                    panic!("Can only rotate two or more values");
                }

                let mut values = Vec::new();

                // Pop all values from stack:
                for _ in 0..*amount {
                    values.push(self.pop_value());
                }

                // Push top of stack back first:
                self.push_value(values.remove(0));

                // Push other value back in order:
                values.reverse();
                for value in values {
                    self.push_value(value);
                }
                None
            }
            bytecode::Instruction::BuildList { size } => {
                let elements = self.pop_multiple(*size);
                let list_obj = vm.ctx.new_list(elements);
                self.push_value(list_obj);
                None
            }
            bytecode::Instruction::BuildSet { size } => {
                let elements = self.pop_multiple(*size);
                let py_obj = vm.ctx.new_set(elements);
                self.push_value(py_obj);
                None
            }
            bytecode::Instruction::BuildTuple { size } => {
                let elements = self.pop_multiple(*size);
                let list_obj = vm.ctx.new_tuple(elements);
                self.push_value(list_obj);
                None
            }
            bytecode::Instruction::BuildMap { size } => {
                let mut elements = HashMap::new();
                for _x in 0..*size {
                    let obj = self.pop_value();
                    // XXX: Currently, we only support String keys, so we have to unwrap the
                    // PyObject (and ensure it is a String).
                    let key_pyobj = self.pop_value();
                    let key = match key_pyobj.borrow().kind {
                        PyObjectKind::String { ref value } => value.clone(),
                        ref kind => unimplemented!(
                            "Only strings can be used as dict keys, we saw: {:?}",
                            kind
                        ),
                    };
                    elements.insert(key, obj);
                }
                let map_obj = PyObject::new(
                    PyObjectKind::Dict { elements: elements },
                    vm.ctx.dict_type(),
                );
                self.push_value(map_obj);
                None
            }
            bytecode::Instruction::BuildSlice { size } => {
                assert!(*size == 2 || *size == 3);
                let elements = self.pop_multiple(*size);

                let mut out: Vec<Option<i32>> = elements
                    .into_iter()
                    .map(|x| match x.borrow().kind {
                        PyObjectKind::Integer { value } => Some(value),
                        PyObjectKind::None => None,
                        _ => panic!("Expect Int or None as BUILD_SLICE arguments, got {:?}", x),
                    })
                    .collect();

                let start = out[0];
                let stop = out[1];
                let step = if out.len() == 3 { out[2] } else { None };

                let obj = PyObject::new(
                    PyObjectKind::Slice { start, stop, step },
                    vm.ctx.type_type(),
                );
                self.push_value(obj);
                None
            }
            bytecode::Instruction::ListAppend { i } => {
                let list_obj = self.nth_value(*i);
                let item = self.pop_value();
                // TODO: objlist::list_append()
                match vm.call_method(&list_obj, "append", vec![item]) {
                    Ok(_) => None,
                    Err(err) => Some(Err(err)),
                }
            }
            bytecode::Instruction::SetAdd { i } => {
                let set_obj = self.nth_value(*i);
                let item = self.pop_value();
                match vm.call_method(&set_obj, "add", vec![item]) {
                    Ok(_) => None,
                    Err(err) => Some(Err(err)),
                }
            }
            bytecode::Instruction::MapAdd { i } => {
                let dict_obj = self.nth_value(*i + 1);
                let key = self.pop_value();
                let value = self.pop_value();
                match vm.call_method(&dict_obj, "__setitem__", vec![key, value]) {
                    Ok(_) => None,
                    Err(err) => Some(Err(err)),
                }
            }
            bytecode::Instruction::BinaryOperation { ref op } => self.execute_binop(vm, op),
            bytecode::Instruction::LoadAttr { ref name } => self.load_attr(vm, name),
            bytecode::Instruction::StoreAttr { ref name } => self.store_attr(name),
            bytecode::Instruction::DeleteAttr { ref name } => self.delete_attr(vm, name),
            bytecode::Instruction::UnaryOperation { ref op } => self.execute_unop(vm, op),
            bytecode::Instruction::CompareOperation { ref op } => self.execute_compare(vm, op),
            bytecode::Instruction::ReturnValue => {
                let value = self.pop_value();
                if let Some(exc) = self.unwind_blocks(vm) {
                    Some(Err(exc))
                } else {
                    Some(Ok(ExecutionResult::Return(value)))
                }
            }
            bytecode::Instruction::YieldValue => {
                let value = self.pop_value();
                Some(Ok(ExecutionResult::Yield(value)))
            }
            bytecode::Instruction::SetupLoop { start, end } => {
                self.push_block(Block::Loop {
                    start: *start,
                    end: *end,
                });
                None
            }
            bytecode::Instruction::SetupExcept { handler } => {
                self.push_block(Block::TryExcept { handler: *handler });
                None
            }
            bytecode::Instruction::SetupWith { end } => {
                let context_manager = self.pop_value();
                // Call enter:
                match vm.call_method(&context_manager, "__enter__", vec![]) {
                    Ok(obj) => {
                        self.push_block(Block::With {
                            end: *end,
                            context_manager: context_manager.clone(),
                        });
                        self.push_value(obj);
                        None
                    }
                    Err(err) => Some(Err(err)),
                }
            }
            bytecode::Instruction::CleanupWith { end: end1 } => {
                let block = self.pop_block().unwrap();
                if let Block::With {
                    end: end2,
                    context_manager,
                } = &block
                {
                    assert!(end1 == end2);

                    // call exit now with no exception:
                    match self.with_exit(vm, context_manager, None) {
                        Ok(..) => None,
                        Err(exc) => Some(Err(exc)),
                    }
                } else {
                    panic!("Block stack is incorrect, expected a with block");
                }
            }
            bytecode::Instruction::PopBlock => {
                self.pop_block();
                None
            }
            bytecode::Instruction::GetIter => {
                let iterated_obj = self.pop_value();
                match objiter::get_iter(vm, &iterated_obj) {
                    Ok(iter_obj) => {
                        self.push_value(iter_obj);
                        None
                    }
                    Err(err) => Some(Err(err)),
                }
            }
            bytecode::Instruction::ForIter => {
                // The top of stack contains the iterator, lets push it forward:
                let top_of_stack = self.last_value();
                let next_obj: PyResult = vm.call_method(&top_of_stack, "__next__", vec![]);

                // Check the next object:
                match next_obj {
                    Ok(value) => {
                        self.push_value(value);
                        None
                    }
                    Err(next_error) => {
                        // Check if we have stopiteration, or something else:
                        if objtype::isinstance(
                            &next_error,
                            vm.ctx.exceptions.stop_iteration.clone(),
                        ) {
                            // Pop iterator from stack:
                            self.pop_value();

                            // End of for loop
                            let end_label = if let Block::Loop { start: _, end } = self.last_block()
                            {
                                *end
                            } else {
                                panic!("Wrong block type")
                            };
                            self.jump(end_label);
                            None
                        } else {
                            Some(Err(next_error))
                        }
                    }
                }
            }
            bytecode::Instruction::MakeFunction { flags } => {
                let _qualified_name = self.pop_value();
                let code_obj = self.pop_value();
                let defaults = if flags.contains(bytecode::FunctionOpArg::HAS_DEFAULTS) {
                    self.pop_value()
                } else {
                    vm.get_none()
                };
                // pop argc arguments
                // argument: name, args, globals
                let scope = self.locals.clone();
                let obj = vm.ctx.new_function(code_obj, scope, defaults);
                self.push_value(obj);
                None
            }
            bytecode::Instruction::CallFunction { count } => {
                let args: Vec<PyObjectRef> = self.pop_multiple(*count);
                let args = PyFuncArgs {
                    args: args,
                    kwargs: vec![],
                };
                let func_ref = self.pop_value();

                // Call function:
                let func_result = vm.invoke(func_ref, args);

                match func_result {
                    Ok(value) => {
                        self.push_value(value);
                        None
                    }
                    Err(value) => {
                        // Ripple exception upwards:
                        Some(Err(value))
                    }
                }
            }
            bytecode::Instruction::CallFunctionKw { count } => {
                let kwarg_names = self.pop_value();
                let args: Vec<PyObjectRef> = self.pop_multiple(*count);

                let kwarg_names = kwarg_names
                    .to_vec()
                    .unwrap()
                    .iter()
                    .map(|pyobj| objstr::get_value(pyobj))
                    .collect();
                let args = PyFuncArgs::new(args, kwarg_names);
                let func_ref = self.pop_value();

                // Call function:
                let func_result = vm.invoke(func_ref, args);

                match func_result {
                    Ok(value) => {
                        self.push_value(value);
                        None
                    }
                    Err(value) => {
                        // Ripple exception upwards:
                        Some(Err(value))
                    }
                }
            }
            bytecode::Instruction::Jump { target } => {
                self.jump(*target);
                None
            }
            bytecode::Instruction::JumpIf { target } => {
                let obj = self.pop_value();
                match objbool::boolval(vm, obj) {
                    Ok(value) => {
                        if value {
                            self.jump(*target);
                        }
                        None
                    }
                    Err(value) => Some(Err(value)),
                }
            }

            bytecode::Instruction::JumpIfFalse { target } => {
                let obj = self.pop_value();
                match objbool::boolval(vm, obj) {
                    Ok(value) => {
                        if !value {
                            self.jump(*target);
                        }
                        None
                    }
                    Err(value) => Some(Err(value)),
                }
            }

            bytecode::Instruction::Raise { argc } => {
                let exception = match argc {
                    1 => self.pop_value(),
                    0 | 2 | 3 => panic!("Not implemented!"),
                    _ => panic!("Invalid paramter for RAISE_VARARGS, must be between 0 to 3"),
                };
                if objtype::isinstance(&exception, vm.ctx.exceptions.base_exception_type.clone()) {
                    info!("Exception raised: {:?}", exception);
                    Some(Err(exception))
                } else {
                    let msg = format!(
                        "Can only raise BaseException derived types, not {:?}",
                        exception
                    );
                    let type_error_type = vm.ctx.exceptions.type_error.clone();
                    let type_error = vm.new_exception(type_error_type, msg);
                    Some(Err(type_error))
                }
            }

            bytecode::Instruction::Break => {
                let block = self.unwind_loop(vm);
                if let Block::Loop { start: _, end } = block {
                    self.jump(end);
                }
                None
            }
            bytecode::Instruction::Pass => {
                // Ah, this is nice, just relax!
                None
            }
            bytecode::Instruction::Continue => {
                let block = self.unwind_loop(vm);
                if let Block::Loop { start, end: _ } = block {
                    self.jump(start);
                } else {
                    assert!(false);
                }
                None
            }
            bytecode::Instruction::PrintExpr => {
                let expr = self.pop_value();
                match expr.borrow().kind {
                    PyObjectKind::None => (),
                    _ => {
                        let repr = vm.to_repr(expr.clone()).unwrap();
                        builtins::builtin_print(
                            vm,
                            PyFuncArgs {
                                args: vec![repr],
                                kwargs: vec![],
                            },
                        )
                        .unwrap();
                    }
                }
                None
            }
            bytecode::Instruction::LoadBuildClass => {
                let rustfunc = PyObject::new(
                    PyObjectKind::RustFunction {
                        function: builtins::builtin_build_class_,
                    },
                    vm.ctx.type_type(),
                );
                self.push_value(rustfunc);
                None
            }
            bytecode::Instruction::StoreLocals => {
                let locals = self.pop_value();
                match self.locals.borrow_mut().kind {
                    PyObjectKind::Scope { ref mut scope } => {
                        scope.locals = locals;
                    }
                    _ => panic!("We really expect our scope to be a scope!"),
                }
                None
            }
            bytecode::Instruction::UnpackSequence { size } => {
                let value = self.pop_value();

                let elements = objtuple::get_elements(&value);
                if elements.len() != *size {
                    Some(Err(vm.new_value_error(
                        "Wrong number of values to unpack".to_string(),
                    )))
                } else {
                    for element in elements.into_iter().rev() {
                        self.push_value(element);
                    }
                    None
                }
            }
            bytecode::Instruction::UnpackEx { before, after } => {
                let value = self.pop_value();

                let elements = objtuple::get_elements(&value);
                let min_expected = *before + *after;
                if elements.len() < min_expected {
                    Some(Err(vm.new_value_error(format!(
                        "Not enough values to unpack (expected at least {}, got {}",
                        min_expected,
                        elements.len()
                    ))))
                } else {
                    let middle = elements.len() - *before - *after;

                    // Elements on stack from right-to-left:
                    for element in elements[*before + middle..].iter().rev() {
                        self.push_value(element.clone());
                    }

                    let middle_elements = elements
                        .iter()
                        .skip(*before)
                        .take(middle)
                        .map(|x| x.clone())
                        .collect();
                    let t = vm.ctx.new_list(middle_elements);
                    self.push_value(t);

                    // Lastly the first reversed values:
                    for element in elements[..*before].iter().rev() {
                        self.push_value(element.clone());
                    }

                    None
                }
            }
            bytecode::Instruction::Unpack => {
                let value = self.pop_value();

                let elements = objtuple::get_elements(&value);

                for element in elements.into_iter().rev() {
                    self.push_value(element);
                }
                None
            }
        }
    }

    fn import(
        &mut self,
        vm: &mut VirtualMachine,
        module: &str,
        symbol: &Option<String>,
    ) -> Option<FrameResult> {
        let current_path = match &self.code.source_path {
            Some(source_path) => {
                let mut source_pathbuf = PathBuf::from(source_path);
                source_pathbuf.pop();
                source_pathbuf
            }
            None => PathBuf::from("."),
        };

        let obj = match import(vm, current_path, &module.to_string(), symbol) {
            Ok(value) => value,
            Err(value) => return Some(Err(value)),
        };

        // Push module on stack:
        self.push_value(obj);
        None
    }

    // Unwind all blocks:
    fn unwind_blocks(&mut self, vm: &mut VirtualMachine) -> Option<PyObjectRef> {
        loop {
            let block = self.pop_block();
            match block {
                Some(Block::Loop { .. }) => {}
                Some(Block::TryExcept { .. }) => {
                    // TODO: execute finally handler
                }
                Some(Block::With {
                    end: _,
                    context_manager,
                }) => {
                    match self.with_exit(vm, &context_manager, None) {
                        Ok(..) => {}
                        Err(exc) => {
                            // __exit__ went wrong,
                            return Some(exc);
                        }
                    }
                }
                None => break None,
            }
        }
    }

    fn unwind_loop(&mut self, vm: &mut VirtualMachine) -> Block {
        loop {
            let block = self.pop_block();
            match block {
                Some(Block::Loop { start: _, end: __ }) => break block.unwrap(),
                Some(Block::TryExcept { .. }) => {
                    // TODO: execute finally handler
                }
                Some(Block::With {
                    end: _,
                    context_manager,
                }) => match self.with_exit(vm, &context_manager, None) {
                    Ok(..) => {}
                    Err(exc) => {
                        panic!("Exception in with __exit__ {:?}", exc);
                    }
                },
                None => panic!("No block to break / continue"),
            }
        }
    }

    fn unwind_exception(
        &mut self,
        vm: &mut VirtualMachine,
        exc: PyObjectRef,
    ) -> Option<PyObjectRef> {
        // unwind block stack on exception and find any handlers:
        loop {
            let block = self.pop_block();
            match block {
                Some(Block::TryExcept { handler }) => {
                    self.push_value(exc);
                    self.jump(handler);
                    return None;
                }
                Some(Block::With {
                    end,
                    context_manager,
                }) => {
                    match self.with_exit(vm, &context_manager, Some(exc.clone())) {
                        Ok(exit_action) => {
                            match objbool::boolval(vm, exit_action) {
                                Ok(handle_exception) => {
                                    if handle_exception {
                                        // We handle the exception, so return!
                                        self.jump(end);
                                        return None;
                                    } else {
                                        // go on with the stack unwinding.
                                    }
                                }
                                Err(exit_exc) => {
                                    return Some(exit_exc);
                                }
                            }
                            // if objtype::isinstance
                        }
                        Err(exit_exc) => {
                            // TODO: what about original exception?
                            return Some(exit_exc);
                        }
                    }
                }
                Some(Block::Loop { .. }) => {}
                None => break,
            }
        }
        Some(exc)
    }

    fn with_exit(
        &mut self,
        vm: &mut VirtualMachine,
        context_manager: &PyObjectRef,
        exc: Option<PyObjectRef>,
    ) -> PyResult {
        // Assume top of stack is __exit__ method:
        // TODO: do we want to put the exit call on the stack?
        // let exit_method = self.pop_value();
        // let args = PyFuncArgs::default();
        // TODO: what happens when we got an error during handling exception?
        let args = if let Some(exc) = exc {
            let exc_type = exc.typ();
            let exc_val = exc.clone();
            let exc_tb = vm.ctx.none(); // TODO: retrieve traceback?
            vec![exc_type, exc_val, exc_tb]
        } else {
            let exc_type = vm.ctx.none();
            let exc_val = vm.ctx.none();
            let exc_tb = vm.ctx.none();
            vec![exc_type, exc_val, exc_tb]
        };
        vm.call_method(context_manager, "__exit__", args)
    }

    fn store_name(&mut self, name: &str) -> Option<FrameResult> {
        let obj = self.pop_value();
        self.locals.set_item(name, obj);
        None
    }

    fn delete_name(&mut self, vm: &mut VirtualMachine, name: &str) -> Option<FrameResult> {
        let locals = match self.locals.borrow().kind {
            PyObjectKind::Scope { ref scope } => scope.locals.clone(),
            _ => panic!("We really expect our scope to be a scope!"),
        };

        // Assume here that locals is a dict
        let name = vm.ctx.new_str(name.to_string());
        match vm.call_method(&locals, "__delitem__", vec![name]) {
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        }
    }

    fn load_name(&mut self, vm: &mut VirtualMachine, name: &str) -> Option<FrameResult> {
        // Lookup name in scope and put it onto the stack!
        let mut scope = self.locals.clone();
        loop {
            if scope.contains_key(name) {
                let obj = scope.get_item(name).unwrap();
                self.push_value(obj);
                break None;
            } else if scope.has_parent() {
                scope = scope.get_parent();
            } else {
                let name_error_type = vm.ctx.exceptions.name_error.clone();
                let msg = format!("Has not attribute '{}'", name);
                let name_error = vm.new_exception(name_error_type, msg);
                break Some(Err(name_error));
            }
        }
    }

    fn subscript(&mut self, vm: &mut VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        vm.call_method(&a, "__getitem__", vec![b])
    }

    fn execute_store_subscript(&mut self, vm: &mut VirtualMachine) -> Option<FrameResult> {
        let idx = self.pop_value();
        let obj = self.pop_value();
        let value = self.pop_value();
        let a2 = &mut *obj.borrow_mut();
        let result = match &mut a2.kind {
            PyObjectKind::List { ref mut elements } => objlist::set_item(vm, elements, idx, value),
            _ => Err(vm.new_type_error(format!(
                "TypeError: __setitem__ assign type {:?} with index {:?} is not supported (yet?)",
                obj, idx
            ))),
        };

        match result {
            Ok(_) => None,
            Err(value) => Some(Err(value)),
        }
    }

    fn execute_delete_subscript(&mut self, vm: &mut VirtualMachine) -> Option<FrameResult> {
        let idx = self.pop_value();
        let obj = self.pop_value();
        match vm.call_method(&obj, "__delitem__", vec![idx]) {
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        }
    }

    fn jump(&mut self, label: bytecode::Label) {
        let target_pc = self.code.label_map[&label];
        trace!("program counter from {:?} to {:?}", self.lasti, target_pc);
        self.lasti = target_pc;
    }

    fn execute_binop(
        &mut self,
        vm: &mut VirtualMachine,
        op: &bytecode::BinaryOperator,
    ) -> Option<FrameResult> {
        let b_ref = self.pop_value();
        let a_ref = self.pop_value();
        let result = match op {
            &bytecode::BinaryOperator::Subtract => vm._sub(a_ref, b_ref),
            &bytecode::BinaryOperator::Add => vm._add(a_ref, b_ref),
            &bytecode::BinaryOperator::Multiply => vm._mul(a_ref, b_ref),
            &bytecode::BinaryOperator::MatrixMultiply => {
                vm.call_method(&a_ref, "__matmul__", vec![b_ref])
            }
            &bytecode::BinaryOperator::Power => vm._pow(a_ref, b_ref),
            &bytecode::BinaryOperator::Divide => vm._div(a_ref, b_ref),
            &bytecode::BinaryOperator::FloorDivide => {
                vm.call_method(&a_ref, "__floordiv__", vec![b_ref])
            }
            &bytecode::BinaryOperator::Subscript => self.subscript(vm, a_ref, b_ref),
            &bytecode::BinaryOperator::Modulo => vm._modulo(a_ref, b_ref),
            &bytecode::BinaryOperator::Lshift => vm.call_method(&a_ref, "__lshift__", vec![b_ref]),
            &bytecode::BinaryOperator::Rshift => vm.call_method(&a_ref, "__rshift__", vec![b_ref]),
            &bytecode::BinaryOperator::Xor => vm._xor(a_ref, b_ref),
            &bytecode::BinaryOperator::Or => vm._or(a_ref, b_ref),
            &bytecode::BinaryOperator::And => vm._and(a_ref, b_ref),
        };
        match result {
            Ok(value) => {
                self.push_value(value);
                None
            }
            Err(value) => Some(Err(value)),
        }
    }

    fn execute_unop(
        &mut self,
        vm: &mut VirtualMachine,
        op: &bytecode::UnaryOperator,
    ) -> Option<FrameResult> {
        let a = self.pop_value();
        let result = match op {
            &bytecode::UnaryOperator::Minus => {
                // TODO:
                // self.invoke('__neg__'
                match a.borrow().kind {
                    PyObjectKind::Integer { value: ref value1 } => Ok(vm.ctx.new_int(-*value1)),
                    PyObjectKind::Float { value: ref value1 } => Ok(vm.ctx.new_float(-*value1)),
                    _ => panic!("Not impl {:?}", a),
                }
            }
            &bytecode::UnaryOperator::Not => match objbool::boolval(vm, a) {
                Ok(result) => Ok(vm.ctx.new_bool(!result)),
                Err(err) => Err(err),
            },
            _ => panic!("Not impl {:?}", op),
        };
        match result {
            Ok(value) => {
                self.push_value(value);
                None
            }
            Err(value) => Some(Err(value)),
        }
    }

    fn _eq(&mut self, vm: &mut VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        vm.call_method(&a, "__eq__", vec![b])
    }

    fn _ne(&mut self, vm: &mut VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        vm.call_method(&a, "__ne__", vec![b])
    }

    fn _lt(&mut self, vm: &mut VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        vm.call_method(&a, "__lt__", vec![b])
    }

    fn _le(&mut self, vm: &mut VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        vm.call_method(&a, "__le__", vec![b])
    }

    fn _gt(&mut self, vm: &mut VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        vm.call_method(&a, "__gt__", vec![b])
    }

    fn _ge(&mut self, vm: &mut VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        vm.call_method(&a, "__ge__", vec![b])
    }

    fn _id(&self, a: PyObjectRef) -> usize {
        a.get_id()
    }

    // https://docs.python.org/3/reference/expressions.html#membership-test-operations
    fn _membership(
        &mut self,
        vm: &mut VirtualMachine,
        needle: PyObjectRef,
        haystack: &PyObjectRef,
    ) -> PyResult {
        vm.call_method(&haystack, "__contains__", vec![needle])
        // TODO: implement __iter__ and __getitem__ cases when __contains__ is
        // not implemented.
    }

    fn _in(
        &mut self,
        vm: &mut VirtualMachine,
        needle: PyObjectRef,
        haystack: PyObjectRef,
    ) -> PyResult {
        match self._membership(vm, needle, &haystack) {
            Ok(found) => Ok(found),
            Err(_) => Err(vm.new_type_error(format!(
                "{} has no __contains__ method",
                objtype::get_type_name(&haystack.typ())
            ))),
        }
    }

    fn _not_in(
        &mut self,
        vm: &mut VirtualMachine,
        needle: PyObjectRef,
        haystack: PyObjectRef,
    ) -> PyResult {
        match self._membership(vm, needle, &haystack) {
            Ok(found) => Ok(vm.ctx.new_bool(!objbool::get_value(&found))),
            Err(_) => Err(vm.new_type_error(format!(
                "{} has no __contains__ method",
                objtype::get_type_name(&haystack.typ())
            ))),
        }
    }

    fn _is(&self, a: PyObjectRef, b: PyObjectRef) -> bool {
        // Pointer equal:
        a.is(&b)
    }

    fn _is_not(&self, vm: &VirtualMachine, a: PyObjectRef, b: PyObjectRef) -> PyResult {
        let result_bool = !a.is(&b);
        let result = vm.ctx.new_bool(result_bool);
        Ok(result)
    }

    fn execute_compare(
        &mut self,
        vm: &mut VirtualMachine,
        op: &bytecode::ComparisonOperator,
    ) -> Option<FrameResult> {
        let b = self.pop_value();
        let a = self.pop_value();
        let result = match op {
            &bytecode::ComparisonOperator::Equal => self._eq(vm, a, b),
            &bytecode::ComparisonOperator::NotEqual => self._ne(vm, a, b),
            &bytecode::ComparisonOperator::Less => self._lt(vm, a, b),
            &bytecode::ComparisonOperator::LessOrEqual => self._le(vm, a, b),
            &bytecode::ComparisonOperator::Greater => self._gt(vm, a, b),
            &bytecode::ComparisonOperator::GreaterOrEqual => self._ge(vm, a, b),
            &bytecode::ComparisonOperator::Is => Ok(vm.ctx.new_bool(self._is(a, b))),
            &bytecode::ComparisonOperator::IsNot => self._is_not(vm, a, b),
            &bytecode::ComparisonOperator::In => self._in(vm, a, b),
            &bytecode::ComparisonOperator::NotIn => self._not_in(vm, a, b),
        };

        match result {
            Ok(value) => {
                self.push_value(value);
                None
            }
            Err(value) => Some(Err(value)),
        }
    }

    fn load_attr(&mut self, vm: &mut VirtualMachine, attr_name: &str) -> Option<FrameResult> {
        let parent = self.pop_value();
        match vm.get_attribute(parent, attr_name) {
            Ok(obj) => {
                self.push_value(obj);
                None
            }
            Err(err) => Some(Err(err)),
        }
    }

    fn store_attr(&mut self, attr_name: &str) -> Option<FrameResult> {
        let parent = self.pop_value();
        let value = self.pop_value();
        parent.set_attr(attr_name, value);
        None
    }

    fn delete_attr(&mut self, vm: &mut VirtualMachine, attr_name: &str) -> Option<FrameResult> {
        let parent = self.pop_value();
        let name = vm.ctx.new_str(attr_name.to_string());
        match vm.call_method(&parent, "__delattr__", vec![name]) {
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        }
    }

    fn unwrap_constant(&self, vm: &VirtualMachine, value: &bytecode::Constant) -> PyObjectRef {
        match *value {
            bytecode::Constant::Integer { ref value } => vm.ctx.new_int(*value),
            bytecode::Constant::Float { ref value } => vm.ctx.new_float(*value),
            bytecode::Constant::String { ref value } => vm.new_str(value.clone()),
            bytecode::Constant::Boolean { ref value } => vm.new_bool(value.clone()),
            bytecode::Constant::Code { ref code } => {
                PyObject::new(PyObjectKind::Code { code: code.clone() }, vm.get_type())
            }
            bytecode::Constant::Tuple { ref elements } => vm.ctx.new_tuple(
                elements
                    .iter()
                    .map(|value| self.unwrap_constant(vm, value))
                    .collect(),
            ),
            bytecode::Constant::None => vm.ctx.none(),
        }
    }

    pub fn get_lineno(&self) -> ast::Location {
        self.code.locations[self.lasti].clone()
    }

    fn push_block(&mut self, block: Block) {
        self.blocks.push(block);
    }

    fn pop_block(&mut self) -> Option<Block> {
        self.blocks.pop()
    }

    fn last_block(&self) -> &Block {
        self.blocks.last().unwrap()
    }

    pub fn push_value(&mut self, obj: PyObjectRef) {
        self.stack.push(obj);
    }

    fn pop_value(&mut self) -> PyObjectRef {
        self.stack.pop().unwrap()
    }

    fn pop_multiple(&mut self, count: usize) -> Vec<PyObjectRef> {
        let mut objs: Vec<PyObjectRef> = Vec::new();
        for _x in 0..count {
            objs.push(self.stack.pop().unwrap());
        }
        objs.reverse();
        objs
    }

    fn last_value(&self) -> PyObjectRef {
        self.stack.last().unwrap().clone()
    }

    fn nth_value(&self, depth: usize) -> PyObjectRef {
        self.stack[self.stack.len() - depth - 1].clone()
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let stack_str = self
            .stack
            .iter()
            .map(|elem| format!("\n  > {}", elem.borrow().str()))
            .collect::<Vec<_>>()
            .join("");
        let block_str = self
            .blocks
            .iter()
            .map(|elem| format!("\n  > {:?}", elem))
            .collect::<Vec<_>>()
            .join("");
        let local_str = match self.locals.borrow().kind {
            PyObjectKind::Scope { ref scope } => match scope.locals.borrow().kind {
                PyObjectKind::Dict { ref elements } => elements
                    .iter()
                    .map(|elem| format!("\n  {} = {}", elem.0, elem.1.borrow().str()))
                    .collect::<Vec<_>>()
                    .join(""),
                ref unexpected => panic!(
                    "locals unexpectedly not wrapping a dict! instead: {:?}",
                    unexpected
                ),
            },
            ref unexpected => panic!("locals unexpectedly not a scope! instead: {:?}", unexpected),
        };
        write!(
            f,
            "Frame Object {{ \n Stack:{}\n Blocks:{}\n Locals:{}\n}}",
            stack_str, block_str, local_str
        )
    }
}
