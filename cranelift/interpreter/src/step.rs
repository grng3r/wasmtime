//! The [step] function interprets a single Cranelift instruction given its [State] and
//! [InstructionContext]; the interpretation is generic over [Value]s.
use crate::address::{Address, AddressSize};
use crate::instruction::InstructionContext;
use crate::state::{InterpreterFunctionRef, MemoryError, State};
use crate::value::{Value, ValueConversionKind, ValueError, ValueResult};
use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, AbiParam, AtomicRmwOp, Block, BlockCall, ExternalName, FuncRef, Function,
    InstructionData, MemFlags, Opcode, TrapCode, Type, Value as ValueRef,
};
use log::trace;
use smallvec::{smallvec, SmallVec};
use std::convert::{TryFrom, TryInto};
use std::fmt::Debug;
use std::ops::RangeFrom;
use thiserror::Error;

/// Ensures that all types in args are the same as expected by the signature
fn validate_signature_params(sig: &[AbiParam], args: &[impl Value]) -> bool {
    args.iter()
        .map(|r| r.ty())
        .zip(sig.iter().map(|r| r.value_type))
        .all(|(a, b)| match (a, b) {
            // For these two cases we don't have precise type information for `a`.
            // We don't distinguish between different bool types, or different vector types
            // The actual error is in `Value::ty` that returns default types for some values
            // but we don't have enough information there either.
            //
            // Ideally the user has run the verifier and caught this properly...
            (a, b) if a.is_vector() && b.is_vector() => true,
            (a, b) => a == b,
        })
}

// Helper for summing a sequence of values.
fn sum<V: Value>(head: V, tail: SmallVec<[V; 1]>) -> ValueResult<i128> {
    let mut acc = head;
    for t in tail {
        acc = Value::add(acc, t)?;
    }
    acc.into_int()
}

/// Interpret a single Cranelift instruction. Note that program traps and interpreter errors are
/// distinct: a program trap results in `Ok(Flow::Trap(...))` whereas an interpretation error (e.g.
/// the types of two values are incompatible) results in `Err(...)`.
#[allow(unused_variables)]
pub fn step<'a, V, I>(
    state: &mut dyn State<'a, V>,
    inst_context: I,
) -> Result<ControlFlow<'a, V>, StepError>
where
    V: Value + Debug,
    I: InstructionContext,
{
    let inst = inst_context.data();
    let ctrl_ty = inst_context.controlling_type().unwrap();
    trace!(
        "Step: {}{}",
        inst.opcode(),
        if ctrl_ty.is_invalid() {
            String::new()
        } else {
            format!(".{}", ctrl_ty)
        }
    );

    // The following closures make the `step` implementation much easier to express. Note that they
    // frequently close over the `state` or `inst_context` for brevity.

    // Retrieve the current value for an instruction argument.
    let arg = |index: usize| -> Result<V, StepError> {
        let value_ref = inst_context.args()[index];
        state
            .get_value(value_ref)
            .ok_or(StepError::UnknownValue(value_ref))
    };

    // Retrieve the current values for all of an instruction's arguments.
    let args = || -> Result<SmallVec<[V; 1]>, StepError> {
        state
            .collect_values(inst_context.args())
            .map_err(|v| StepError::UnknownValue(v))
    };

    // Retrieve the current values for a range of an instruction's arguments.
    let args_range = |indexes: RangeFrom<usize>| -> Result<SmallVec<[V; 1]>, StepError> {
        Ok(SmallVec::<[V; 1]>::from(&args()?[indexes]))
    };

    // Retrieve the immediate value for an instruction, expecting it to exist.
    let imm = || -> V {
        V::from(match inst {
            InstructionData::UnaryConst {
                constant_handle, ..
            } => {
                let buffer = state
                    .get_current_function()
                    .dfg
                    .constants
                    .get(constant_handle.clone())
                    .as_slice();
                match ctrl_ty.bytes() {
                    16 => DataValue::V128(buffer.try_into().expect("a 16-byte data buffer")),
                    8 => DataValue::V64(buffer.try_into().expect("an 8-byte data buffer")),
                    length => panic!("unexpected UnaryConst buffer length {}", length),
                }
            }
            InstructionData::Shuffle { imm, .. } => {
                let mask = state
                    .get_current_function()
                    .dfg
                    .immediates
                    .get(imm)
                    .unwrap()
                    .as_slice();
                match mask.len() {
                    16 => DataValue::V128(mask.try_into().expect("a 16-byte vector mask")),
                    8 => DataValue::V64(mask.try_into().expect("an 8-byte vector mask")),
                    length => panic!("unexpected Shuffle mask length {}", mask.len()),
                }
            }
            // 8-bit.
            InstructionData::BinaryImm8 { imm, .. } | InstructionData::TernaryImm8 { imm, .. } => {
                DataValue::from(imm as i8) // Note the switch from unsigned to signed.
            }
            // 32-bit
            InstructionData::UnaryIeee32 { imm, .. } => DataValue::from(imm),
            InstructionData::Load { offset, .. }
            | InstructionData::Store { offset, .. }
            | InstructionData::StackLoad { offset, .. }
            | InstructionData::StackStore { offset, .. }
            | InstructionData::TableAddr { offset, .. } => DataValue::from(offset),
            // 64-bit.
            InstructionData::UnaryImm { imm, .. }
            | InstructionData::BinaryImm64 { imm, .. }
            | InstructionData::IntCompareImm { imm, .. } => DataValue::from(imm.bits()),
            InstructionData::UnaryIeee64 { imm, .. } => DataValue::from(imm),
            _ => unreachable!(),
        })
    };

    // Retrieve the immediate value for an instruction and convert it to the controlling type of the
    // instruction. For example, since `InstructionData` stores all integer immediates in a 64-bit
    // size, this will attempt to convert `iconst.i8 ...` to an 8-bit size.
    let imm_as_ctrl_ty =
        || -> Result<V, ValueError> { V::convert(imm(), ValueConversionKind::Exact(ctrl_ty)) };

    // Indicate that the result of a step is to assign a single value to an instruction's results.
    let assign = |value: V| ControlFlow::Assign(smallvec![value]);

    // Indicate that the result of a step is to assign multiple values to an instruction's results.
    let assign_multiple = |values: &[V]| ControlFlow::Assign(SmallVec::from(values));

    // Similar to `assign` but converts some errors into traps
    let assign_or_trap = |value: ValueResult<V>| match value {
        Ok(v) => Ok(assign(v)),
        Err(ValueError::IntegerDivisionByZero) => Ok(ControlFlow::Trap(CraneliftTrap::User(
            TrapCode::IntegerDivisionByZero,
        ))),
        Err(ValueError::IntegerOverflow) => Ok(ControlFlow::Trap(CraneliftTrap::User(
            TrapCode::IntegerOverflow,
        ))),
        Err(e) => Err(e),
    };

    let memerror_to_trap = |e: MemoryError| match e {
        MemoryError::InvalidAddress(_) => TrapCode::HeapOutOfBounds,
        MemoryError::InvalidAddressType(_) => TrapCode::HeapOutOfBounds,
        MemoryError::InvalidOffset { .. } => TrapCode::HeapOutOfBounds,
        MemoryError::InvalidEntry { .. } => TrapCode::HeapOutOfBounds,
        MemoryError::OutOfBoundsStore { .. } => TrapCode::HeapOutOfBounds,
        MemoryError::OutOfBoundsLoad { .. } => TrapCode::HeapOutOfBounds,
        MemoryError::MisalignedLoad { .. } => TrapCode::HeapMisaligned,
        MemoryError::MisalignedStore { .. } => TrapCode::HeapMisaligned,
    };

    // Assigns or traps depending on the value of the result
    let assign_or_memtrap = |res| match res {
        Ok(v) => assign(v),
        Err(e) => ControlFlow::Trap(CraneliftTrap::User(memerror_to_trap(e))),
    };

    // Continues or traps depending on the value of the result
    let continue_or_memtrap = |res| match res {
        Ok(_) => ControlFlow::Continue,
        Err(e) => ControlFlow::Trap(CraneliftTrap::User(memerror_to_trap(e))),
    };

    let calculate_addr = |addr_ty: Type, imm: V, args: SmallVec<[V; 1]>| -> ValueResult<u64> {
        let imm = imm.convert(ValueConversionKind::ZeroExtend(addr_ty))?;
        let args = args
            .into_iter()
            .map(|v| v.convert(ValueConversionKind::ZeroExtend(addr_ty)))
            .collect::<ValueResult<SmallVec<[V; 1]>>>()?;

        Ok(sum(imm, args)? as u64)
    };

    // Interpret a unary instruction with the given `op`, assigning the resulting value to the
    // instruction's results.
    let unary = |op: fn(V) -> ValueResult<V>, arg: V| -> ValueResult<ControlFlow<V>> {
        let ctrl_ty = inst_context.controlling_type().unwrap();
        let res = unary_arith(arg, ctrl_ty, op, false)?;
        Ok(assign(res))
    };

    // Interpret a binary instruction with the given `op`, assigning the resulting value to the
    // instruction's results.
    let binary =
        |op: fn(V, V) -> ValueResult<V>, left: V, right: V| -> ValueResult<ControlFlow<V>> {
            let ctrl_ty = inst_context.controlling_type().unwrap();
            let res = binary_arith(left, right, ctrl_ty, op, false)?;
            Ok(assign(res))
        };

    // Same as `binary_unsigned`, but converts the values to their unsigned form before the
    // operation and back to signed form afterwards. Since Cranelift types have no notion of
    // signedness, this enables operations that depend on sign.
    let binary_unsigned =
        |op: fn(V, V) -> ValueResult<V>, left: V, right: V| -> ValueResult<ControlFlow<V>> {
            let ctrl_ty = inst_context.controlling_type().unwrap();
            let res = binary_arith(left, right, ctrl_ty, op, true)
                .and_then(|v| v.convert(ValueConversionKind::ToSigned))?;
            Ok(assign(res))
        };

    // Similar to `binary` but converts select `ValueError`'s into trap `ControlFlow`'s
    let binary_can_trap =
        |op: fn(V, V) -> ValueResult<V>, left: V, right: V| -> ValueResult<ControlFlow<V>> {
            let ctrl_ty = inst_context.controlling_type().unwrap();
            let res = binary_arith(left, right, ctrl_ty, op, false);
            assign_or_trap(res)
        };

    // Same as `binary_can_trap`, but converts the values to their unsigned form before the
    // operation and back to signed form afterwards. Since Cranelift types have no notion of
    // signedness, this enables operations that depend on sign.
    let binary_unsigned_can_trap =
        |op: fn(V, V) -> ValueResult<V>, left: V, right: V| -> ValueResult<ControlFlow<V>> {
            let ctrl_ty = inst_context.controlling_type().unwrap();
            let res = binary_arith(left, right, ctrl_ty, op, true)
                .and_then(|v| v.convert(ValueConversionKind::ToSigned));
            assign_or_trap(res)
        };

    // Choose whether to assign `left` or `right` to the instruction's result based on a `condition`.
    let choose = |condition: bool, left: V, right: V| -> ControlFlow<V> {
        assign(if condition { left } else { right })
    };

    // Retrieve an instruction's branch destination; expects the instruction to be a branch.

    let continue_at = |block: BlockCall| {
        let branch_args = state
            .collect_values(block.args_slice(&state.get_current_function().dfg.value_lists))
            .map_err(|v| StepError::UnknownValue(v))?;
        Ok(ControlFlow::ContinueAt(
            block.block(&state.get_current_function().dfg.value_lists),
            branch_args,
        ))
    };

    // Based on `condition`, indicate where to continue the control flow.
    let branch_when = |condition: bool, block| -> Result<ControlFlow<V>, StepError> {
        if condition {
            continue_at(block)
        } else {
            Ok(ControlFlow::Continue)
        }
    };

    // Retrieve an instruction's trap code; expects the instruction to be a trap.
    let trap_code = || -> TrapCode { inst.trap_code().unwrap() };

    // Based on `condition`, either trap or not.
    let trap_when = |condition: bool, trap: CraneliftTrap| -> ControlFlow<V> {
        if condition {
            ControlFlow::Trap(trap)
        } else {
            ControlFlow::Continue
        }
    };

    // Calls a function reference with the given arguments.
    let call_func = |func_ref: InterpreterFunctionRef<'a>,
                     args: SmallVec<[V; 1]>,
                     make_ctrl_flow: fn(&'a Function, SmallVec<[V; 1]>) -> ControlFlow<'a, V>|
     -> Result<ControlFlow<'a, V>, StepError> {
        let signature = func_ref.signature();

        // Check the types of the arguments. This is usually done by the verifier, but nothing
        // guarantees that the user has ran that.
        let args_match = validate_signature_params(&signature.params[..], &args[..]);
        if !args_match {
            return Ok(ControlFlow::Trap(CraneliftTrap::User(
                TrapCode::BadSignature,
            )));
        }

        Ok(match func_ref {
            InterpreterFunctionRef::Function(func) => make_ctrl_flow(func, args),
            InterpreterFunctionRef::LibCall(libcall) => {
                debug_assert!(
                    !matches!(
                        inst.opcode(),
                        Opcode::ReturnCall | Opcode::ReturnCallIndirect,
                    ),
                    "Cannot tail call to libcalls"
                );
                let libcall_handler = state.get_libcall_handler();

                // We don't transfer control to a libcall, we just execute it and return the results
                let res = libcall_handler(libcall, args);
                let res = match res {
                    Err(trap) => return Ok(ControlFlow::Trap(CraneliftTrap::User(trap))),
                    Ok(rets) => rets,
                };

                // Check that what the handler returned is what we expect.
                if validate_signature_params(&signature.returns[..], &res[..]) {
                    ControlFlow::Assign(res)
                } else {
                    ControlFlow::Trap(CraneliftTrap::User(TrapCode::BadSignature))
                }
            }
        })
    };

    // Interpret a Cranelift instruction.
    Ok(match inst.opcode() {
        Opcode::Jump => {
            if let InstructionData::Jump { destination, .. } = inst {
                continue_at(destination)?
            } else {
                unreachable!()
            }
        }
        Opcode::Brif => {
            if let InstructionData::Brif {
                arg,
                blocks: [block_then, block_else],
                ..
            } = inst
            {
                let arg = state.get_value(arg).ok_or(StepError::UnknownValue(arg))?;

                let condition = arg.convert(ValueConversionKind::ToBoolean)?.into_bool()?;

                if condition {
                    continue_at(block_then)?
                } else {
                    continue_at(block_else)?
                }
            } else {
                unreachable!()
            }
        }
        Opcode::BrTable => {
            if let InstructionData::BranchTable { table, .. } = inst {
                let jt_data = &state.get_current_function().stencil.dfg.jump_tables[table];

                // Convert to usize to remove negative indexes from the following operations
                let jump_target = usize::try_from(arg(0)?.into_int()?)
                    .ok()
                    .and_then(|i| jt_data.as_slice().get(i))
                    .copied()
                    .unwrap_or(jt_data.default_block());

                continue_at(jump_target)?
            } else {
                unreachable!()
            }
        }
        Opcode::Trap => ControlFlow::Trap(CraneliftTrap::User(trap_code())),
        Opcode::Debugtrap => ControlFlow::Trap(CraneliftTrap::Debug),
        Opcode::ResumableTrap => ControlFlow::Trap(CraneliftTrap::Resumable),
        Opcode::Trapz => trap_when(!arg(0)?.into_bool()?, CraneliftTrap::User(trap_code())),
        Opcode::Trapnz => trap_when(arg(0)?.into_bool()?, CraneliftTrap::User(trap_code())),
        Opcode::ResumableTrapnz => trap_when(arg(0)?.into_bool()?, CraneliftTrap::Resumable),
        Opcode::Return => ControlFlow::Return(args()?),
        Opcode::Call | Opcode::ReturnCall => {
            let func_ref = if let InstructionData::Call { func_ref, .. } = inst {
                func_ref
            } else {
                unreachable!()
            };

            let curr_func = state.get_current_function();
            let ext_data = curr_func
                .dfg
                .ext_funcs
                .get(func_ref)
                .ok_or(StepError::UnknownFunction(func_ref))?;

            let args = args()?;
            let func = match ext_data.name {
                // These functions should be registered in the regular function store
                ExternalName::User(_) | ExternalName::TestCase(_) => {
                    let function = state
                        .get_function(func_ref)
                        .ok_or(StepError::UnknownFunction(func_ref))?;
                    InterpreterFunctionRef::Function(function)
                }
                ExternalName::LibCall(libcall) => InterpreterFunctionRef::LibCall(libcall),
                ExternalName::KnownSymbol(_) => unimplemented!(),
            };

            let make_control_flow = match inst.opcode() {
                Opcode::Call => ControlFlow::Call,
                Opcode::ReturnCall => ControlFlow::ReturnCall,
                _ => unreachable!(),
            };

            call_func(func, args, make_control_flow)?
        }
        Opcode::CallIndirect | Opcode::ReturnCallIndirect => {
            let args = args()?;
            let addr_dv = DataValue::U64(arg(0)?.into_int()? as u64);
            let addr = Address::try_from(addr_dv.clone()).map_err(StepError::MemoryError)?;

            let func = state
                .get_function_from_address(addr)
                .ok_or_else(|| StepError::MemoryError(MemoryError::InvalidAddress(addr_dv)))?;

            let call_args: SmallVec<[V; 1]> = SmallVec::from(&args[1..]);

            let make_control_flow = match inst.opcode() {
                Opcode::CallIndirect => ControlFlow::Call,
                Opcode::ReturnCallIndirect => ControlFlow::ReturnCall,
                _ => unreachable!(),
            };

            call_func(func, call_args, make_control_flow)?
        }
        Opcode::FuncAddr => {
            let func_ref = if let InstructionData::FuncAddr { func_ref, .. } = inst {
                func_ref
            } else {
                unreachable!()
            };

            let ext_data = state
                .get_current_function()
                .dfg
                .ext_funcs
                .get(func_ref)
                .ok_or(StepError::UnknownFunction(func_ref))?;

            let addr_ty = inst_context.controlling_type().unwrap();
            assign_or_memtrap({
                AddressSize::try_from(addr_ty).and_then(|addr_size| {
                    let addr = state.function_address(addr_size, &ext_data.name)?;
                    let dv = DataValue::try_from(addr)?;
                    Ok(dv.into())
                })
            })
        }
        Opcode::Load
        | Opcode::Uload8
        | Opcode::Sload8
        | Opcode::Uload16
        | Opcode::Sload16
        | Opcode::Uload32
        | Opcode::Sload32
        | Opcode::Uload8x8
        | Opcode::Sload8x8
        | Opcode::Uload16x4
        | Opcode::Sload16x4
        | Opcode::Uload32x2
        | Opcode::Sload32x2 => {
            let ctrl_ty = inst_context.controlling_type().unwrap();
            let (load_ty, kind) = match inst.opcode() {
                Opcode::Load => (ctrl_ty, None),
                Opcode::Uload8 => (types::I8, Some(ValueConversionKind::ZeroExtend(ctrl_ty))),
                Opcode::Sload8 => (types::I8, Some(ValueConversionKind::SignExtend(ctrl_ty))),
                Opcode::Uload16 => (types::I16, Some(ValueConversionKind::ZeroExtend(ctrl_ty))),
                Opcode::Sload16 => (types::I16, Some(ValueConversionKind::SignExtend(ctrl_ty))),
                Opcode::Uload32 => (types::I32, Some(ValueConversionKind::ZeroExtend(ctrl_ty))),
                Opcode::Sload32 => (types::I32, Some(ValueConversionKind::SignExtend(ctrl_ty))),
                Opcode::Uload8x8
                | Opcode::Sload8x8
                | Opcode::Uload16x4
                | Opcode::Sload16x4
                | Opcode::Uload32x2
                | Opcode::Sload32x2 => unimplemented!(),
                _ => unreachable!(),
            };

            let addr_value = calculate_addr(types::I64, imm(), args()?)?;
            let mem_flags = inst.memflags().expect("instruction to have memory flags");
            let loaded = assign_or_memtrap(
                Address::try_from(addr_value)
                    .and_then(|addr| state.checked_load(addr, load_ty, mem_flags)),
            );

            match (loaded, kind) {
                (ControlFlow::Assign(ret), Some(c)) => ControlFlow::Assign(
                    ret.into_iter()
                        .map(|loaded| loaded.convert(c.clone()))
                        .collect::<ValueResult<SmallVec<[V; 1]>>>()?,
                ),
                (cf, _) => cf,
            }
        }
        Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32 => {
            let kind = match inst.opcode() {
                Opcode::Store => None,
                Opcode::Istore8 => Some(ValueConversionKind::Truncate(types::I8)),
                Opcode::Istore16 => Some(ValueConversionKind::Truncate(types::I16)),
                Opcode::Istore32 => Some(ValueConversionKind::Truncate(types::I32)),
                _ => unreachable!(),
            };

            let addr_value = calculate_addr(types::I64, imm(), args_range(1..)?)?;
            let mem_flags = inst.memflags().expect("instruction to have memory flags");
            let reduced = if let Some(c) = kind {
                arg(0)?.convert(c)?
            } else {
                arg(0)?
            };
            continue_or_memtrap(
                Address::try_from(addr_value)
                    .and_then(|addr| state.checked_store(addr, reduced, mem_flags)),
            )
        }
        Opcode::StackLoad => {
            let load_ty = inst_context.controlling_type().unwrap();
            let slot = inst.stack_slot().unwrap();
            let offset = sum(imm(), args()?)? as u64;
            let mem_flags = MemFlags::new();
            assign_or_memtrap({
                state
                    .stack_address(AddressSize::_64, slot, offset)
                    .and_then(|addr| state.checked_load(addr, load_ty, mem_flags))
            })
        }
        Opcode::StackStore => {
            let arg = arg(0)?;
            let slot = inst.stack_slot().unwrap();
            let offset = sum(imm(), args_range(1..)?)? as u64;
            let mem_flags = MemFlags::new();
            continue_or_memtrap({
                state
                    .stack_address(AddressSize::_64, slot, offset)
                    .and_then(|addr| state.checked_store(addr, arg, mem_flags))
            })
        }
        Opcode::StackAddr => {
            let load_ty = inst_context.controlling_type().unwrap();
            let slot = inst.stack_slot().unwrap();
            let offset = sum(imm(), args()?)? as u64;
            assign_or_memtrap({
                AddressSize::try_from(load_ty).and_then(|addr_size| {
                    let addr = state.stack_address(addr_size, slot, offset)?;
                    let dv = DataValue::try_from(addr)?;
                    Ok(dv.into())
                })
            })
        }
        Opcode::DynamicStackAddr => unimplemented!("DynamicStackSlot"),
        Opcode::DynamicStackLoad => unimplemented!("DynamicStackLoad"),
        Opcode::DynamicStackStore => unimplemented!("DynamicStackStore"),
        Opcode::GlobalValue => {
            if let InstructionData::UnaryGlobalValue { global_value, .. } = inst {
                assign_or_memtrap(state.resolve_global_value(global_value))
            } else {
                unreachable!()
            }
        }
        Opcode::SymbolValue => unimplemented!("SymbolValue"),
        Opcode::TlsValue => unimplemented!("TlsValue"),
        Opcode::GetPinnedReg => assign(state.get_pinned_reg()),
        Opcode::SetPinnedReg => {
            let arg0 = arg(0)?;
            state.set_pinned_reg(arg0);
            ControlFlow::Continue
        }
        Opcode::TableAddr => {
            if let InstructionData::TableAddr { table, offset, .. } = inst {
                let table = &state.get_current_function().tables[table];
                let base = state.resolve_global_value(table.base_gv)?;
                let bound = state.resolve_global_value(table.bound_gv)?;
                let index_ty = table.index_type;
                let element_size = V::int(u64::from(table.element_size) as i128, index_ty)?;
                let inst_offset = V::int(i32::from(offset) as i128, index_ty)?;

                let byte_offset = arg(0)?.mul(element_size.clone())?.add(inst_offset)?;
                let bound_bytes = bound.mul(element_size)?;
                if byte_offset.gt(&bound_bytes)? {
                    return Ok(ControlFlow::Trap(CraneliftTrap::User(
                        TrapCode::HeapOutOfBounds,
                    )));
                }

                assign(base.add(byte_offset)?)
            } else {
                unreachable!()
            }
        }
        Opcode::Iconst => assign(Value::int(imm().into_int()?, ctrl_ty)?),
        Opcode::F32const => assign(imm()),
        Opcode::F64const => assign(imm()),
        Opcode::Vconst => assign(imm()),
        Opcode::Null => unimplemented!("Null"),
        Opcode::Nop => ControlFlow::Continue,
        Opcode::Select | Opcode::SelectSpectreGuard => {
            choose(arg(0)?.into_bool()?, arg(1)?, arg(2)?)
        }
        Opcode::Bitselect => assign(bitselect(arg(0)?, arg(1)?, arg(2)?)?),
        Opcode::Icmp => assign(icmp(
            ctrl_ty,
            inst.cond_code().unwrap(),
            &arg(0)?,
            &arg(1)?,
        )?),
        Opcode::IcmpImm => assign(icmp(
            ctrl_ty,
            inst.cond_code().unwrap(),
            &arg(0)?,
            &imm_as_ctrl_ty()?,
        )?),
        Opcode::Smin => {
            if ctrl_ty.is_vector() {
                let icmp = icmp(ctrl_ty, IntCC::SignedGreaterThan, &arg(1)?, &arg(0)?)?;
                assign(bitselect(icmp, arg(0)?, arg(1)?)?)
            } else {
                choose(Value::gt(&arg(1)?, &arg(0)?)?, arg(0)?, arg(1)?)
            }
        }
        Opcode::Umin => {
            if ctrl_ty.is_vector() {
                let icmp = icmp(ctrl_ty, IntCC::UnsignedGreaterThan, &arg(1)?, &arg(0)?)?;
                assign(bitselect(icmp, arg(0)?, arg(1)?)?)
            } else {
                choose(
                    Value::gt(
                        &arg(1)?.convert(ValueConversionKind::ToUnsigned)?,
                        &arg(0)?.convert(ValueConversionKind::ToUnsigned)?,
                    )?,
                    arg(0)?,
                    arg(1)?,
                )
            }
        }
        Opcode::Smax => {
            if ctrl_ty.is_vector() {
                let icmp = icmp(ctrl_ty, IntCC::SignedGreaterThan, &arg(0)?, &arg(1)?)?;
                assign(bitselect(icmp, arg(0)?, arg(1)?)?)
            } else {
                choose(Value::gt(&arg(0)?, &arg(1)?)?, arg(0)?, arg(1)?)
            }
        }
        Opcode::Umax => {
            if ctrl_ty.is_vector() {
                let icmp = icmp(ctrl_ty, IntCC::UnsignedGreaterThan, &arg(0)?, &arg(1)?)?;
                assign(bitselect(icmp, arg(0)?, arg(1)?)?)
            } else {
                choose(
                    Value::gt(
                        &arg(0)?.convert(ValueConversionKind::ToUnsigned)?,
                        &arg(1)?.convert(ValueConversionKind::ToUnsigned)?,
                    )?,
                    arg(0)?,
                    arg(1)?,
                )
            }
        }
        Opcode::AvgRound => {
            let sum = Value::add(arg(0)?, arg(1)?)?;
            let one = Value::int(1, arg(0)?.ty())?;
            let inc = Value::add(sum, one)?;
            let two = Value::int(2, arg(0)?.ty())?;
            binary(Value::div, inc, two)?
        }
        Opcode::Iadd => binary(Value::add, arg(0)?, arg(1)?)?,
        Opcode::UaddSat => assign(binary_arith(
            arg(0)?,
            arg(1)?,
            ctrl_ty,
            Value::add_sat,
            true,
        )?),
        Opcode::SaddSat => assign(binary_arith(
            arg(0)?,
            arg(1)?,
            ctrl_ty,
            Value::add_sat,
            false,
        )?),
        Opcode::Isub => binary(Value::sub, arg(0)?, arg(1)?)?,
        Opcode::UsubSat => assign(binary_arith(
            arg(0)?,
            arg(1)?,
            ctrl_ty,
            Value::sub_sat,
            true,
        )?),
        Opcode::SsubSat => assign(binary_arith(
            arg(0)?,
            arg(1)?,
            ctrl_ty,
            Value::sub_sat,
            false,
        )?),
        Opcode::Ineg => binary(Value::sub, Value::int(0, ctrl_ty)?, arg(0)?)?,
        Opcode::Iabs => {
            let (min_val, _) = ctrl_ty.lane_type().bounds(true);
            let min_val: V = Value::int(min_val as i128, ctrl_ty.lane_type())?;
            let arg0 = extractlanes(&arg(0)?, ctrl_ty)?;
            let new_vec = arg0
                .into_iter()
                .map(|lane| {
                    if Value::eq(&lane, &min_val)? {
                        Ok(min_val.clone())
                    } else {
                        Value::int(lane.into_int()?.abs(), ctrl_ty.lane_type())
                    }
                })
                .collect::<ValueResult<SimdVec<V>>>()?;
            assign(vectorizelanes(&new_vec, ctrl_ty)?)
        }
        Opcode::Imul => binary(Value::mul, arg(0)?, arg(1)?)?,
        Opcode::Umulhi | Opcode::Smulhi => {
            let double_length = match ctrl_ty.lane_bits() {
                8 => types::I16,
                16 => types::I32,
                32 => types::I64,
                64 => types::I128,
                _ => unimplemented!("Unsupported integer length {}", ctrl_ty.bits()),
            };
            let conv_type = if inst.opcode() == Opcode::Umulhi {
                ValueConversionKind::ZeroExtend(double_length)
            } else {
                ValueConversionKind::SignExtend(double_length)
            };
            let arg0 = extractlanes(&arg(0)?, ctrl_ty)?;
            let arg1 = extractlanes(&arg(1)?, ctrl_ty)?;

            let res = arg0
                .into_iter()
                .zip(arg1)
                .map(|(x, y)| {
                    let x = x.convert(conv_type.clone())?;
                    let y = y.convert(conv_type.clone())?;

                    Ok(Value::mul(x, y)?
                        .convert(ValueConversionKind::ExtractUpper(ctrl_ty.lane_type()))?)
                })
                .collect::<ValueResult<SimdVec<V>>>()?;

            assign(vectorizelanes(&res, ctrl_ty)?)
        }
        Opcode::Udiv => binary_unsigned_can_trap(Value::div, arg(0)?, arg(1)?)?,
        Opcode::Sdiv => binary_can_trap(Value::div, arg(0)?, arg(1)?)?,
        Opcode::Urem => binary_unsigned_can_trap(Value::rem, arg(0)?, arg(1)?)?,
        Opcode::Srem => binary_can_trap(Value::rem, arg(0)?, arg(1)?)?,
        Opcode::IaddImm => binary(Value::add, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::ImulImm => binary(Value::mul, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::UdivImm => binary_unsigned_can_trap(Value::div, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::SdivImm => binary_can_trap(Value::div, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::UremImm => binary_unsigned_can_trap(Value::rem, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::SremImm => binary_can_trap(Value::rem, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::IrsubImm => binary(Value::sub, imm_as_ctrl_ty()?, arg(0)?)?,
        Opcode::IaddCin => choose(
            Value::into_bool(arg(2)?)?,
            Value::add(Value::add(arg(0)?, arg(1)?)?, Value::int(1, ctrl_ty)?)?,
            Value::add(arg(0)?, arg(1)?)?,
        ),
        Opcode::IaddCout => {
            let carry = arg(0)?.checked_add(arg(1)?)?.is_none();
            let sum = arg(0)?.add(arg(1)?)?;
            assign_multiple(&[sum, Value::bool(carry, false, types::I8)?])
        }
        Opcode::IaddCarry => {
            let mut sum = Value::add(arg(0)?, arg(1)?)?;
            let mut carry = arg(0)?.checked_add(arg(1)?)?.is_none();

            if Value::into_bool(arg(2)?)? {
                carry |= sum.clone().checked_add(Value::int(1, ctrl_ty)?)?.is_none();
                sum = Value::add(sum, Value::int(1, ctrl_ty)?)?;
            }

            assign_multiple(&[sum, Value::bool(carry, false, types::I8)?])
        }
        Opcode::UaddOverflowTrap => {
            let sum = Value::add(arg(0)?, arg(1)?)?;
            let carry = Value::lt(&sum, &arg(0)?)? && Value::lt(&sum, &arg(1)?)?;
            if carry {
                ControlFlow::Trap(CraneliftTrap::User(trap_code()))
            } else {
                assign(sum)
            }
        }
        Opcode::IsubBin => choose(
            Value::into_bool(arg(2)?)?,
            Value::sub(arg(0)?, Value::add(arg(1)?, Value::int(1, ctrl_ty)?)?)?,
            Value::sub(arg(0)?, arg(1)?)?,
        ),
        Opcode::IsubBout => {
            let sum = Value::sub(arg(0)?, arg(1)?)?;
            let borrow = Value::lt(&arg(0)?, &arg(1)?)?;
            assign_multiple(&[sum, Value::bool(borrow, false, types::I8)?])
        }
        Opcode::IsubBorrow => {
            let rhs = if Value::into_bool(arg(2)?)? {
                Value::add(arg(1)?, Value::int(1, ctrl_ty)?)?
            } else {
                arg(1)?
            };
            let borrow = Value::lt(&arg(0)?, &rhs)?;
            let sum = Value::sub(arg(0)?, rhs)?;
            assign_multiple(&[sum, Value::bool(borrow, false, types::I8)?])
        }
        Opcode::Band => binary(Value::and, arg(0)?, arg(1)?)?,
        Opcode::Bor => binary(Value::or, arg(0)?, arg(1)?)?,
        Opcode::Bxor => binary(Value::xor, arg(0)?, arg(1)?)?,
        Opcode::Bnot => unary(Value::not, arg(0)?)?,
        Opcode::BandNot => binary(Value::and, arg(0)?, Value::not(arg(1)?)?)?,
        Opcode::BorNot => binary(Value::or, arg(0)?, Value::not(arg(1)?)?)?,
        Opcode::BxorNot => binary(Value::xor, arg(0)?, Value::not(arg(1)?)?)?,
        Opcode::BandImm => binary(Value::and, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::BorImm => binary(Value::or, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::BxorImm => binary(Value::xor, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::Rotl => binary(Value::rotl, arg(0)?, arg(1)?)?,
        Opcode::Rotr => binary(Value::rotr, arg(0)?, arg(1)?)?,
        Opcode::RotlImm => binary(Value::rotl, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::RotrImm => binary(Value::rotr, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::Ishl => binary(Value::shl, arg(0)?, arg(1)?)?,
        Opcode::Ushr => binary_unsigned(Value::ushr, arg(0)?, arg(1)?)?,
        Opcode::Sshr => binary(Value::ishr, arg(0)?, arg(1)?)?,
        Opcode::IshlImm => binary(Value::shl, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::UshrImm => binary_unsigned(Value::ushr, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::SshrImm => binary(Value::ishr, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::Bitrev => unary(Value::reverse_bits, arg(0)?)?,
        Opcode::Bswap => unary(Value::swap_bytes, arg(0)?)?,
        Opcode::Clz => unary(Value::leading_zeros, arg(0)?)?,
        Opcode::Cls => {
            let count = if Value::lt(&arg(0)?, &Value::int(0, ctrl_ty)?)? {
                arg(0)?.leading_ones()?
            } else {
                arg(0)?.leading_zeros()?
            };
            assign(Value::sub(count, Value::int(1, ctrl_ty)?)?)
        }
        Opcode::Ctz => unary(Value::trailing_zeros, arg(0)?)?,
        Opcode::Popcnt => {
            let count = if arg(0)?.ty().is_int() {
                arg(0)?.count_ones()?
            } else {
                let lanes = extractlanes(&arg(0)?, ctrl_ty)?
                    .into_iter()
                    .map(|lane| lane.count_ones())
                    .collect::<ValueResult<SimdVec<V>>>()?;
                vectorizelanes(&lanes, ctrl_ty)?
            };
            assign(count)
        }

        Opcode::Fcmp => {
            let arg0 = extractlanes(&arg(0)?, ctrl_ty)?;
            let arg1 = extractlanes(&arg(1)?, ctrl_ty)?;

            assign(vectorizelanes(
                &(arg0
                    .into_iter()
                    .zip(arg1.into_iter())
                    .map(|(x, y)| {
                        V::bool(
                            fcmp(inst.fp_cond_code().unwrap(), &x, &y).unwrap(),
                            ctrl_ty.is_vector(),
                            ctrl_ty.lane_type().as_truthy(),
                        )
                    })
                    .collect::<ValueResult<SimdVec<V>>>()?),
                ctrl_ty,
            )?)
        }
        Opcode::Fadd => binary(Value::add, arg(0)?, arg(1)?)?,
        Opcode::Fsub => binary(Value::sub, arg(0)?, arg(1)?)?,
        Opcode::Fmul => binary(Value::mul, arg(0)?, arg(1)?)?,
        Opcode::Fdiv => binary(Value::div, arg(0)?, arg(1)?)?,
        Opcode::Sqrt => unary(Value::sqrt, arg(0)?)?,
        Opcode::Fma => {
            let arg0 = extractlanes(&arg(0)?, ctrl_ty)?;
            let arg1 = extractlanes(&arg(1)?, ctrl_ty)?;
            let arg2 = extractlanes(&arg(2)?, ctrl_ty)?;

            assign(vectorizelanes(
                &(arg0
                    .into_iter()
                    .zip(arg1.into_iter())
                    .zip(arg2.into_iter())
                    .map(|((x, y), z)| Value::fma(x, y, z))
                    .collect::<ValueResult<SimdVec<V>>>()?),
                ctrl_ty,
            )?)
        }
        Opcode::Fneg => unary(Value::neg, arg(0)?)?,
        Opcode::Fabs => unary(Value::abs, arg(0)?)?,
        Opcode::Fcopysign => binary(Value::copysign, arg(0)?, arg(1)?)?,
        Opcode::Fmin => assign(match (arg(0)?, arg(1)?) {
            (a, _) if a.is_nan()? => a,
            (_, b) if b.is_nan()? => b,
            (a, b) if a.is_zero()? && b.is_zero()? && a.is_negative()? => a,
            (a, b) if a.is_zero()? && b.is_zero()? && b.is_negative()? => b,
            (a, b) => a.min(b)?,
        }),
        Opcode::FminPseudo => assign(match (arg(0)?, arg(1)?) {
            (a, b) if a.is_nan()? || b.is_nan()? => a,
            (a, b) if a.is_zero()? && b.is_zero()? => a,
            (a, b) => a.min(b)?,
        }),
        Opcode::Fmax => assign(match (arg(0)?, arg(1)?) {
            (a, _) if a.is_nan()? => a,
            (_, b) if b.is_nan()? => b,
            (a, b) if a.is_zero()? && b.is_zero()? && a.is_negative()? => b,
            (a, b) if a.is_zero()? && b.is_zero()? && b.is_negative()? => a,
            (a, b) => a.max(b)?,
        }),
        Opcode::FmaxPseudo => assign(match (arg(0)?, arg(1)?) {
            (a, b) if a.is_nan()? || b.is_nan()? => a,
            (a, b) if a.is_zero()? && b.is_zero()? => a,
            (a, b) => a.max(b)?,
        }),
        Opcode::Ceil => unary(Value::ceil, arg(0)?)?,
        Opcode::Floor => unary(Value::floor, arg(0)?)?,
        Opcode::Trunc => unary(Value::trunc, arg(0)?)?,
        Opcode::Nearest => unary(Value::nearest, arg(0)?)?,
        Opcode::IsNull => unimplemented!("IsNull"),
        Opcode::IsInvalid => unimplemented!("IsInvalid"),
        Opcode::Bitcast | Opcode::ScalarToVector => {
            let input_ty = inst_context.type_of(inst_context.args()[0]).unwrap();
            let arg0 = extractlanes(&arg(0)?, input_ty)?;
            let lanes = &arg0
                .into_iter()
                .map(|x| V::convert(x, ValueConversionKind::Exact(ctrl_ty.lane_type())))
                .collect::<ValueResult<SimdVec<V>>>()?;
            assign(match inst.opcode() {
                Opcode::Bitcast => vectorizelanes(lanes, ctrl_ty)?,
                Opcode::ScalarToVector => vectorizelanes_all(lanes, ctrl_ty)?,
                _ => unreachable!(),
            })
        }
        Opcode::Ireduce => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::Truncate(ctrl_ty),
        )?),
        Opcode::Snarrow | Opcode::Unarrow | Opcode::Uunarrow => {
            let arg0 = extractlanes(&arg(0)?, ctrl_ty)?;
            let arg1 = extractlanes(&arg(1)?, ctrl_ty)?;
            let new_type = ctrl_ty.split_lanes().unwrap();
            let (min, max) = new_type.bounds(inst.opcode() == Opcode::Snarrow);
            let mut min: V = Value::int(min as i128, ctrl_ty.lane_type())?;
            let mut max: V = Value::int(max as i128, ctrl_ty.lane_type())?;
            if inst.opcode() == Opcode::Uunarrow {
                min = min.convert(ValueConversionKind::ToUnsigned)?;
                max = max.convert(ValueConversionKind::ToUnsigned)?;
            }
            let narrow = |mut lane: V| -> ValueResult<V> {
                if inst.opcode() == Opcode::Uunarrow {
                    lane = lane.convert(ValueConversionKind::ToUnsigned)?;
                }
                lane = Value::max(lane, min.clone())?;
                lane = Value::min(lane, max.clone())?;
                lane = lane.convert(ValueConversionKind::Truncate(new_type.lane_type()))?;
                if inst.opcode() == Opcode::Unarrow || inst.opcode() == Opcode::Uunarrow {
                    lane = lane.convert(ValueConversionKind::ToUnsigned)?;
                }
                Ok(lane)
            };
            let new_vec = arg0
                .into_iter()
                .chain(arg1)
                .map(|lane| narrow(lane))
                .collect::<ValueResult<Vec<_>>>()?;
            assign(vectorizelanes(&new_vec, new_type)?)
        }
        Opcode::Bmask => assign({
            let bool = arg(0)?;
            let bool_ty = ctrl_ty.as_truthy_pedantic();
            let lanes = extractlanes(&bool, bool_ty)?
                .into_iter()
                .map(|lane| lane.convert(ValueConversionKind::Mask(ctrl_ty.lane_type())))
                .collect::<ValueResult<SimdVec<V>>>()?;
            vectorizelanes(&lanes, ctrl_ty)?
        }),
        Opcode::Sextend => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::SignExtend(ctrl_ty),
        )?),
        Opcode::Uextend => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::ZeroExtend(ctrl_ty),
        )?),
        Opcode::Fpromote => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::Exact(ctrl_ty),
        )?),
        Opcode::Fdemote => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::RoundNearestEven(ctrl_ty),
        )?),
        Opcode::Shuffle => {
            let mask = imm().into_array()?;
            let a = Value::into_array(&arg(0)?)?;
            let b = Value::into_array(&arg(1)?)?;
            let mut new = [0u8; 16];
            for i in 0..mask.len() {
                if (mask[i] as usize) < a.len() {
                    new[i] = a[mask[i] as usize];
                } else if (mask[i] as usize - a.len()) < b.len() {
                    new[i] = b[mask[i] as usize - a.len()];
                } // else leave as 0.
            }
            assign(Value::vector(new, types::I8X16)?)
        }
        Opcode::Swizzle => {
            let x = Value::into_array(&arg(0)?)?;
            let s = Value::into_array(&arg(1)?)?;
            let mut new = [0u8; 16];
            for i in 0..new.len() {
                if (s[i] as usize) < new.len() {
                    new[i] = x[s[i] as usize];
                } // else leave as 0
            }
            assign(Value::vector(new, types::I8X16)?)
        }
        Opcode::Splat => {
            let mut new_vector = SimdVec::new();
            for _ in 0..ctrl_ty.lane_count() {
                new_vector.push(arg(0)?);
            }
            assign(vectorizelanes(&new_vector, ctrl_ty)?)
        }
        Opcode::Insertlane => {
            let idx = imm().into_int()? as usize;
            let mut vector = extractlanes(&arg(0)?, ctrl_ty)?;
            vector[idx] = arg(1)?;
            assign(vectorizelanes(&vector, ctrl_ty)?)
        }
        Opcode::Extractlane => {
            let idx = imm().into_int()? as usize;
            let lanes = extractlanes(&arg(0)?, ctrl_ty)?;
            assign(lanes[idx].clone())
        }
        Opcode::VhighBits => {
            // `ctrl_ty` controls the return type for this, so the input type
            // must be retrieved via `inst_context`.
            let vector_type = inst_context.type_of(inst_context.args()[0]).unwrap();
            let a = extractlanes(&arg(0)?, vector_type)?;
            let mut result: i128 = 0;
            for (i, val) in a.into_iter().enumerate() {
                let val = val.reverse_bits()?.into_int()?; // MSB -> LSB
                result |= (val & 1) << i;
            }
            assign(Value::int(result, ctrl_ty)?)
        }
        Opcode::VanyTrue => {
            let lane_ty = ctrl_ty.lane_type();
            let init = V::bool(false, true, lane_ty)?;
            let any = fold_vector(arg(0)?, ctrl_ty, init.clone(), |acc, lane| acc.or(lane))?;
            assign(V::bool(!V::eq(&any, &init)?, false, types::I8)?)
        }
        Opcode::VallTrue => {
            let lane_ty = ctrl_ty.lane_type();
            let init = V::bool(true, true, lane_ty)?;
            let all = fold_vector(arg(0)?, ctrl_ty, init.clone(), |acc, lane| acc.and(lane))?;
            assign(V::bool(V::eq(&all, &init)?, false, types::I8)?)
        }
        Opcode::SwidenLow | Opcode::SwidenHigh | Opcode::UwidenLow | Opcode::UwidenHigh => {
            let new_type = ctrl_ty.merge_lanes().unwrap();
            let conv_type = match inst.opcode() {
                Opcode::SwidenLow | Opcode::SwidenHigh => {
                    ValueConversionKind::SignExtend(new_type.lane_type())
                }
                Opcode::UwidenLow | Opcode::UwidenHigh => {
                    ValueConversionKind::ZeroExtend(new_type.lane_type())
                }
                _ => unreachable!(),
            };
            let vec_iter = extractlanes(&arg(0)?, ctrl_ty)?.into_iter();
            let new_vec = match inst.opcode() {
                Opcode::SwidenLow | Opcode::UwidenLow => vec_iter
                    .take(new_type.lane_count() as usize)
                    .map(|lane| lane.convert(conv_type.clone()))
                    .collect::<ValueResult<Vec<_>>>()?,
                Opcode::SwidenHigh | Opcode::UwidenHigh => vec_iter
                    .skip(new_type.lane_count() as usize)
                    .map(|lane| lane.convert(conv_type.clone()))
                    .collect::<ValueResult<Vec<_>>>()?,
                _ => unreachable!(),
            };
            assign(vectorizelanes(&new_vec, new_type)?)
        }
        Opcode::FcvtToUint | Opcode::FcvtToSint => {
            // NaN check
            if arg(0)?.is_nan()? {
                return Ok(ControlFlow::Trap(CraneliftTrap::User(
                    TrapCode::BadConversionToInteger,
                )));
            }
            let x = arg(0)?.into_float()? as i128;
            let is_signed = inst.opcode() == Opcode::FcvtToSint;
            let (min, max) = ctrl_ty.bounds(is_signed);
            let overflow = if is_signed {
                x < (min as i128) || x > (max as i128)
            } else {
                x < 0 || (x as u128) > (max as u128)
            };
            // bounds check
            if overflow {
                return Ok(ControlFlow::Trap(CraneliftTrap::User(
                    TrapCode::IntegerOverflow,
                )));
            }
            // perform the conversion.
            assign(Value::int(x, ctrl_ty)?)
        }
        Opcode::FcvtToUintSat | Opcode::FcvtToSintSat => {
            let in_ty = inst_context.type_of(inst_context.args()[0]).unwrap();
            let cvt = |x: V| -> ValueResult<V> {
                // NaN check
                if x.is_nan()? {
                    V::int(0, ctrl_ty.lane_type())
                } else {
                    let is_signed = inst.opcode() == Opcode::FcvtToSintSat;
                    let (min, max) = ctrl_ty.bounds(is_signed);
                    let x = x.into_float()? as i128;
                    let x = if is_signed {
                        let x = i128::max(x, min as i128);
                        let x = i128::min(x, max as i128);
                        x
                    } else {
                        let x = if x < 0 { 0 } else { x };
                        let x = u128::min(x as u128, max as u128);
                        x as i128
                    };

                    V::int(x, ctrl_ty.lane_type())
                }
            };

            let x = extractlanes(&arg(0)?, in_ty)?;

            assign(vectorizelanes(
                &x.into_iter()
                    .map(cvt)
                    .collect::<ValueResult<SimdVec<V>>>()?,
                ctrl_ty,
            )?)
        }
        Opcode::FcvtFromUint | Opcode::FcvtFromSint => {
            let x = extractlanes(
                &arg(0)?,
                inst_context.type_of(inst_context.args()[0]).unwrap(),
            )?;
            let bits = |x: V| -> ValueResult<u64> {
                let x = if inst.opcode() == Opcode::FcvtFromUint {
                    x.convert(ValueConversionKind::ToUnsigned)?
                } else {
                    x
                };
                Ok(match ctrl_ty.lane_type() {
                    types::F32 => (x.into_int()? as f32).to_bits() as u64,
                    types::F64 => (x.into_int()? as f64).to_bits(),
                    _ => unimplemented!("unexpected conversion to {:?}", ctrl_ty.lane_type()),
                })
            };
            assign(vectorizelanes(
                &x.into_iter()
                    .map(|x| V::float(bits(x)?, ctrl_ty.lane_type()))
                    .collect::<ValueResult<SimdVec<V>>>()?,
                ctrl_ty,
            )?)
        }
        Opcode::FcvtLowFromSint => {
            let in_ty = inst_context.type_of(inst_context.args()[0]).unwrap();
            let x = extractlanes(&arg(0)?, in_ty)?;

            assign(vectorizelanes(
                &(x[..(ctrl_ty.lane_count() as usize)]
                    .into_iter()
                    .map(|x| {
                        V::float(
                            match ctrl_ty.lane_type() {
                                types::F32 => (x.to_owned().into_int()? as f32).to_bits() as u64,
                                types::F64 => (x.to_owned().into_int()? as f64).to_bits(),
                                _ => unimplemented!("unexpected promotion to {:?}", ctrl_ty),
                            },
                            ctrl_ty.lane_type(),
                        )
                    })
                    .collect::<ValueResult<SimdVec<V>>>()?),
                ctrl_ty,
            )?)
        }
        Opcode::FvpromoteLow => {
            let in_ty = inst_context.type_of(inst_context.args()[0]).unwrap();
            assert_eq!(in_ty, types::F32X4);
            let out_ty = types::F64X2;
            let x = extractlanes(&arg(0)?, in_ty)?;
            assign(vectorizelanes(
                &x[..(out_ty.lane_count() as usize)]
                    .into_iter()
                    .map(|x| {
                        V::convert(x.to_owned(), ValueConversionKind::Exact(out_ty.lane_type()))
                    })
                    .collect::<ValueResult<SimdVec<V>>>()?,
                out_ty,
            )?)
        }
        Opcode::Fvdemote => {
            let in_ty = inst_context.type_of(inst_context.args()[0]).unwrap();
            assert_eq!(in_ty, types::F64X2);
            let out_ty = types::F32X4;
            let x = extractlanes(&arg(0)?, in_ty)?;
            let x = &mut x
                .into_iter()
                .map(|x| V::convert(x, ValueConversionKind::RoundNearestEven(out_ty.lane_type())))
                .collect::<ValueResult<SimdVec<V>>>()?;
            // zero the high bits.
            for _ in 0..(out_ty.lane_count() as usize - x.len()) {
                x.push(V::float(0, out_ty.lane_type())?);
            }
            assign(vectorizelanes(x, out_ty)?)
        }
        Opcode::Isplit => assign_multiple(&[
            Value::convert(arg(0)?, ValueConversionKind::Truncate(types::I64))?,
            Value::convert(arg(0)?, ValueConversionKind::ExtractUpper(types::I64))?,
        ]),
        Opcode::Iconcat => assign(Value::concat(arg(0)?, arg(1)?)?),
        Opcode::AtomicRmw => {
            let op = inst.atomic_rmw_op().unwrap();
            let val = arg(1)?;
            let addr = arg(0)?.into_int()? as u64;
            let mem_flags = inst.memflags().expect("instruction to have memory flags");
            let loaded = Address::try_from(addr)
                .and_then(|addr| state.checked_load(addr, ctrl_ty, mem_flags));
            let prev_val = match loaded {
                Ok(v) => v,
                Err(e) => return Ok(ControlFlow::Trap(CraneliftTrap::User(memerror_to_trap(e)))),
            };
            let prev_val_to_assign = prev_val.clone();
            let replace = match op {
                AtomicRmwOp::Xchg => Ok(val),
                AtomicRmwOp::Add => Value::add(prev_val, val),
                AtomicRmwOp::Sub => Value::sub(prev_val, val),
                AtomicRmwOp::And => Value::and(prev_val, val),
                AtomicRmwOp::Or => Value::or(prev_val, val),
                AtomicRmwOp::Xor => Value::xor(prev_val, val),
                AtomicRmwOp::Nand => Value::and(prev_val, val).and_then(V::not),
                AtomicRmwOp::Smax => Value::max(prev_val, val),
                AtomicRmwOp::Smin => Value::min(prev_val, val),
                AtomicRmwOp::Umax => Value::max(
                    Value::convert(val, ValueConversionKind::ToUnsigned)?,
                    Value::convert(prev_val, ValueConversionKind::ToUnsigned)?,
                )
                .and_then(|v| Value::convert(v, ValueConversionKind::ToSigned)),
                AtomicRmwOp::Umin => Value::min(
                    Value::convert(val, ValueConversionKind::ToUnsigned)?,
                    Value::convert(prev_val, ValueConversionKind::ToUnsigned)?,
                )
                .and_then(|v| Value::convert(v, ValueConversionKind::ToSigned)),
            }?;
            let stored = Address::try_from(addr)
                .and_then(|addr| state.checked_store(addr, replace, mem_flags));
            assign_or_memtrap(stored.map(|_| prev_val_to_assign))
        }
        Opcode::AtomicCas => {
            let addr = arg(0)?.into_int()? as u64;
            let mem_flags = inst.memflags().expect("instruction to have memory flags");
            let loaded = Address::try_from(addr)
                .and_then(|addr| state.checked_load(addr, ctrl_ty, mem_flags));
            let loaded_val = match loaded {
                Ok(v) => v,
                Err(e) => return Ok(ControlFlow::Trap(CraneliftTrap::User(memerror_to_trap(e)))),
            };
            let expected_val = arg(1)?;
            let val_to_assign = if Value::eq(&loaded_val, &expected_val)? {
                let val_to_store = arg(2)?;
                Address::try_from(addr)
                    .and_then(|addr| state.checked_store(addr, val_to_store, mem_flags))
                    .map(|_| loaded_val)
            } else {
                Ok(loaded_val)
            };
            assign_or_memtrap(val_to_assign)
        }
        Opcode::AtomicLoad => {
            let load_ty = inst_context.controlling_type().unwrap();
            let addr = arg(0)?.into_int()? as u64;
            let mem_flags = inst.memflags().expect("instruction to have memory flags");
            // We are doing a regular load here, this isn't actually thread safe.
            assign_or_memtrap(
                Address::try_from(addr)
                    .and_then(|addr| state.checked_load(addr, load_ty, mem_flags)),
            )
        }
        Opcode::AtomicStore => {
            let val = arg(0)?;
            let addr = arg(1)?.into_int()? as u64;
            let mem_flags = inst.memflags().expect("instruction to have memory flags");
            // We are doing a regular store here, this isn't actually thread safe.
            continue_or_memtrap(
                Address::try_from(addr).and_then(|addr| state.checked_store(addr, val, mem_flags)),
            )
        }
        Opcode::Fence => {
            // The interpreter always runs in a single threaded context, so we don't
            // actually need to emit a fence here.
            ControlFlow::Continue
        }
        Opcode::SqmulRoundSat => {
            let lane_type = ctrl_ty.lane_type();
            let double_width = ctrl_ty.double_width().unwrap().lane_type();
            let arg0 = extractlanes(&arg(0)?, ctrl_ty)?;
            let arg1 = extractlanes(&arg(1)?, ctrl_ty)?;
            let (min, max) = lane_type.bounds(true);
            let min: V = Value::int(min as i128, double_width)?;
            let max: V = Value::int(max as i128, double_width)?;
            let new_vec = arg0
                .into_iter()
                .zip(arg1.into_iter())
                .map(|(x, y)| {
                    let x = x.into_int()?;
                    let y = y.into_int()?;
                    // temporarily double width of the value to avoid overflow.
                    let z: V = Value::int(
                        (x * y + (1 << (lane_type.bits() - 2))) >> (lane_type.bits() - 1),
                        double_width,
                    )?;
                    // check bounds, saturate, and truncate to correct width.
                    let z = Value::min(z, max.clone())?;
                    let z = Value::max(z, min.clone())?;
                    let z = z.convert(ValueConversionKind::Truncate(lane_type))?;
                    Ok(z)
                })
                .collect::<ValueResult<SimdVec<_>>>()?;
            assign(vectorizelanes(&new_vec, ctrl_ty)?)
        }
        Opcode::IaddPairwise => assign(binary_pairwise(arg(0)?, arg(1)?, ctrl_ty, Value::add)?),
        Opcode::ExtractVector => {
            unimplemented!("ExtractVector not supported");
        }
        Opcode::GetFramePointer => unimplemented!("GetFramePointer"),
        Opcode::GetStackPointer => unimplemented!("GetStackPointer"),
        Opcode::GetReturnAddress => unimplemented!("GetReturnAddress"),
        Opcode::X86Pshufb => unimplemented!("X86Pshufb"),
        Opcode::X86Blendv => unimplemented!("X86Blendv"),
        Opcode::X86Pmulhrsw => unimplemented!("X86Pmulhrsw"),
        Opcode::X86Pmaddubsw => unimplemented!("X86Pmaddubsw"),
        Opcode::X86Cvtt2dq => unimplemented!("X86Cvtt2dq"),
    })
}

#[derive(Error, Debug)]
pub enum StepError {
    #[error("unable to retrieve value from SSA reference: {0}")]
    UnknownValue(ValueRef),
    #[error("unable to find the following function: {0}")]
    UnknownFunction(FuncRef),
    #[error("cannot step with these values")]
    ValueError(#[from] ValueError),
    #[error("failed to access memory")]
    MemoryError(#[from] MemoryError),
}

/// Enumerate the ways in which the control flow can change based on a single step in a Cranelift
/// interpreter.
#[derive(Debug)]
pub enum ControlFlow<'a, V> {
    /// Return one or more values from an instruction to be assigned to a left-hand side, e.g.:
    /// in `v0 = iadd v1, v2`, the sum of `v1` and `v2` is assigned to `v0`.
    Assign(SmallVec<[V; 1]>),
    /// Continue to the next available instruction, e.g.: in `nop`, we expect to resume execution
    /// at the instruction after it.
    Continue,
    /// Jump to another block with the given parameters, e.g.: in
    /// `brif v0, block42(v1, v2), block97`, if the condition is true, we continue execution at the
    /// first instruction of `block42` with the values in `v1` and `v2` filling in the block
    /// parameters.
    ContinueAt(Block, SmallVec<[V; 1]>),
    /// Indicates a call the given [Function] with the supplied arguments.
    Call(&'a Function, SmallVec<[V; 1]>),
    /// Indicates a tail call to the given [Function] with the supplied arguments.
    ReturnCall(&'a Function, SmallVec<[V; 1]>),
    /// Return from the current function with the given parameters, e.g.: `return [v1, v2]`.
    Return(SmallVec<[V; 1]>),
    /// Stop with a program-generated trap; note that these are distinct from errors that may occur
    /// during interpretation.
    Trap(CraneliftTrap),
}

impl<'a, V> ControlFlow<'a, V> {
    /// For convenience, we can unwrap the [ControlFlow] state assuming that it is a
    /// [ControlFlow::Return], panicking otherwise.
    #[cfg(test)]
    pub fn unwrap_return(self) -> Vec<V> {
        if let ControlFlow::Return(values) = self {
            values.into_vec()
        } else {
            panic!("expected the control flow to be in the return state")
        }
    }

    /// For convenience, we can unwrap the [ControlFlow] state assuming that it is a
    /// [ControlFlow::Trap], panicking otherwise.
    #[cfg(test)]
    pub fn unwrap_trap(self) -> CraneliftTrap {
        if let ControlFlow::Trap(trap) = self {
            trap
        } else {
            panic!("expected the control flow to be a trap")
        }
    }
}

#[derive(Error, Debug, PartialEq, Eq, Hash)]
pub enum CraneliftTrap {
    #[error("user code: {0}")]
    User(TrapCode),
    #[error("user debug")]
    Debug,
    #[error("resumable")]
    Resumable,
}

/// Compare two values using the given integer condition `code`.
fn icmp<V>(ctrl_ty: types::Type, code: IntCC, left: &V, right: &V) -> ValueResult<V>
where
    V: Value,
{
    let cmp = |bool_ty: types::Type, code: IntCC, left: &V, right: &V| -> ValueResult<V> {
        Ok(Value::bool(
            match code {
                IntCC::Equal => Value::eq(left, right)?,
                IntCC::NotEqual => !Value::eq(left, right)?,
                IntCC::SignedGreaterThan => Value::gt(left, right)?,
                IntCC::SignedGreaterThanOrEqual => Value::ge(left, right)?,
                IntCC::SignedLessThan => Value::lt(left, right)?,
                IntCC::SignedLessThanOrEqual => Value::le(left, right)?,
                IntCC::UnsignedGreaterThan => Value::gt(
                    &left.clone().convert(ValueConversionKind::ToUnsigned)?,
                    &right.clone().convert(ValueConversionKind::ToUnsigned)?,
                )?,
                IntCC::UnsignedGreaterThanOrEqual => Value::ge(
                    &left.clone().convert(ValueConversionKind::ToUnsigned)?,
                    &right.clone().convert(ValueConversionKind::ToUnsigned)?,
                )?,
                IntCC::UnsignedLessThan => Value::lt(
                    &left.clone().convert(ValueConversionKind::ToUnsigned)?,
                    &right.clone().convert(ValueConversionKind::ToUnsigned)?,
                )?,
                IntCC::UnsignedLessThanOrEqual => Value::le(
                    &left.clone().convert(ValueConversionKind::ToUnsigned)?,
                    &right.clone().convert(ValueConversionKind::ToUnsigned)?,
                )?,
            },
            ctrl_ty.is_vector(),
            bool_ty,
        )?)
    };

    let dst_ty = ctrl_ty.as_truthy();
    let left = extractlanes(left, ctrl_ty)?;
    let right = extractlanes(right, ctrl_ty)?;

    let res = left
        .into_iter()
        .zip(right.into_iter())
        .map(|(l, r)| cmp(dst_ty.lane_type(), code, &l, &r))
        .collect::<ValueResult<SimdVec<V>>>()?;

    Ok(vectorizelanes(&res, dst_ty)?)
}

/// Compare two values using the given floating point condition `code`.
fn fcmp<V>(code: FloatCC, left: &V, right: &V) -> ValueResult<bool>
where
    V: Value,
{
    Ok(match code {
        FloatCC::Ordered => {
            Value::eq(left, right)? || Value::lt(left, right)? || Value::gt(left, right)?
        }
        FloatCC::Unordered => Value::uno(left, right)?,
        FloatCC::Equal => Value::eq(left, right)?,
        FloatCC::NotEqual => {
            Value::lt(left, right)? || Value::gt(left, right)? || Value::uno(left, right)?
        }
        FloatCC::OrderedNotEqual => Value::lt(left, right)? || Value::gt(left, right)?,
        FloatCC::UnorderedOrEqual => Value::eq(left, right)? || Value::uno(left, right)?,
        FloatCC::LessThan => Value::lt(left, right)?,
        FloatCC::LessThanOrEqual => Value::le(left, right)?,
        FloatCC::GreaterThan => Value::gt(left, right)?,
        FloatCC::GreaterThanOrEqual => Value::ge(left, right)?,
        FloatCC::UnorderedOrLessThan => Value::uno(left, right)? || Value::lt(left, right)?,
        FloatCC::UnorderedOrLessThanOrEqual => Value::uno(left, right)? || Value::le(left, right)?,
        FloatCC::UnorderedOrGreaterThan => Value::uno(left, right)? || Value::gt(left, right)?,
        FloatCC::UnorderedOrGreaterThanOrEqual => {
            Value::uno(left, right)? || Value::ge(left, right)?
        }
    })
}

type SimdVec<V> = SmallVec<[V; 4]>;

/// Converts a SIMD vector value into a Rust array of [Value] for processing.
/// If `x` is a scalar, it will be returned as a single-element array.
fn extractlanes<V>(x: &V, vector_type: types::Type) -> ValueResult<SimdVec<V>>
where
    V: Value,
{
    let lane_type = vector_type.lane_type();
    let mut lanes = SimdVec::new();
    // Wrap scalar values as a single-element vector and return.
    if !x.ty().is_vector() {
        lanes.push(x.clone());
        return Ok(lanes);
    }

    let iterations = match lane_type {
        types::I8 => 1,
        types::I16 => 2,
        types::I32 | types::F32 => 4,
        types::I64 | types::F64 => 8,
        _ => unimplemented!("vectors with lanes wider than 64-bits are currently unsupported."),
    };

    let x = x.into_array()?;
    for i in 0..vector_type.lane_count() {
        let mut lane: i128 = 0;
        for j in 0..iterations {
            lane += (x[((i * iterations) + j) as usize] as i128) << (8 * j);
        }

        let lane_val: V = if lane_type.is_float() {
            Value::float(lane as u64, lane_type)?
        } else {
            Value::int(lane, lane_type)?
        };
        lanes.push(lane_val);
    }
    return Ok(lanes);
}

/// Convert a Rust array of [Value] back into a `Value::vector`.
/// Supplying a single-element array will simply return its contained value.
fn vectorizelanes<V>(x: &[V], vector_type: types::Type) -> ValueResult<V>
where
    V: Value,
{
    // If the array is only one element, return it as a scalar.
    if x.len() == 1 {
        Ok(x[0].clone())
    } else {
        vectorizelanes_all(x, vector_type)
    }
}

/// Convert a Rust array of [Value] back into a `Value::vector`.
fn vectorizelanes_all<V>(x: &[V], vector_type: types::Type) -> ValueResult<V>
where
    V: Value,
{
    let lane_type = vector_type.lane_type();
    let iterations = match lane_type {
        types::I8 => 1,
        types::I16 => 2,
        types::I32 | types::F32 => 4,
        types::I64 | types::F64 => 8,
        _ => unimplemented!("vectors with lanes wider than 64-bits are currently unsupported."),
    };
    let mut result: [u8; 16] = [0; 16];
    for (i, val) in x.iter().enumerate() {
        let lane_val: i128 = val
            .clone()
            .convert(ValueConversionKind::Exact(lane_type.as_int()))?
            .into_int()?;

        for j in 0..iterations {
            result[(i * iterations) + j] = (lane_val >> (8 * j)) as u8;
        }
    }
    Value::vector(result, vector_type)
}

/// Performs a lanewise fold on a vector type
fn fold_vector<V, F>(v: V, ty: types::Type, init: V, op: F) -> ValueResult<V>
where
    V: Value,
    F: FnMut(V, V) -> ValueResult<V>,
{
    extractlanes(&v, ty)?.into_iter().try_fold(init, op)
}

/// Performs the supplied unary arithmetic `op` on a Value, either Vector or Scalar.
fn unary_arith<V, F>(x: V, vector_type: types::Type, op: F, unsigned: bool) -> ValueResult<V>
where
    V: Value,
    F: Fn(V) -> ValueResult<V>,
{
    let arg = extractlanes(&x, vector_type)?;

    let result = arg
        .into_iter()
        .map(|mut arg| {
            if unsigned {
                arg = arg.convert(ValueConversionKind::ToUnsigned)?;
            }
            Ok(op(arg)?)
        })
        .collect::<ValueResult<SimdVec<V>>>()?;

    vectorizelanes(&result, vector_type)
}

/// Performs the supplied binary arithmetic `op` on two values, either vector or scalar.
fn binary_arith<V, F>(x: V, y: V, vector_type: types::Type, op: F, unsigned: bool) -> ValueResult<V>
where
    V: Value,
    F: Fn(V, V) -> ValueResult<V>,
{
    let arg0 = extractlanes(&x, vector_type)?;
    let arg1 = extractlanes(&y, vector_type)?;

    let result = arg0
        .into_iter()
        .zip(arg1)
        .map(|(mut lhs, mut rhs)| {
            if unsigned {
                lhs = lhs.convert(ValueConversionKind::ToUnsigned)?;
                rhs = rhs.convert(ValueConversionKind::ToUnsigned)?;
            }
            Ok(op(lhs, rhs)?)
        })
        .collect::<ValueResult<SimdVec<V>>>()?;

    vectorizelanes(&result, vector_type)
}

/// Performs the supplied pairwise arithmetic `op` on two SIMD vectors, where
/// pairs are formed from adjacent vector elements and the vectors are
/// concatenated at the end.
fn binary_pairwise<V, F>(x: V, y: V, vector_type: types::Type, op: F) -> ValueResult<V>
where
    V: Value,
    F: Fn(V, V) -> ValueResult<V>,
{
    let arg0 = extractlanes(&x, vector_type)?;
    let arg1 = extractlanes(&y, vector_type)?;

    let result = arg0
        .chunks(2)
        .chain(arg1.chunks(2))
        .map(|pair| op(pair[0].clone(), pair[1].clone()))
        .collect::<ValueResult<SimdVec<V>>>()?;

    vectorizelanes(&result, vector_type)
}

fn bitselect<V>(c: V, x: V, y: V) -> ValueResult<V>
where
    V: Value,
{
    let mask_x = Value::and(c.clone(), x)?;
    let mask_y = Value::and(Value::not(c)?, y)?;
    Value::or(mask_x, mask_y)
}
