//! The `answer_with` hint each Modbus event carries.
//!
//! Told "ten holding registers at addresses 0 to 9 ... nothing at any other address",
//! llama3.1:8b answers a read of 500-501 by describing the device's whole register map. A read is
//! therefore answered with an address-keyed map and NetGet does the comparison: the hint asks
//! for the registers the device has, names the addresses asked for, and says that one missing
//! from the map is exception 2 - it asks the model to compare nothing. A write carries no values
//! to key, so it keeps the check-first wording. `e2e_test.rs` proves the field reaches the event
//! and that a map without the address becomes exception 2 on the wire.

use netget::server::modbus::actions::answer_with_for_request;
use netget::server::modbus::codec::ModbusRequest;

#[test]
fn a_register_read_asks_for_the_map_and_names_the_addresses() {
    let hint = answer_with_for_request(&ModbusRequest::ReadHoldingRegisters {
        start: 500,
        quantity: 2,
    });
    assert_eq!(
        hint,
        "send_modbus_registers with a registers object holding every holding register your \
         instructions give this device, keyed by address. The client asked for holding \
         registers 500 to 501; NetGet answers exception 2 if any of them is not in your object"
    );
    // No comparison is asked of the model, so no worked comparison and no literal exception.
    assert!(!hint.contains("send_modbus_exception"), "{hint}");
    assert!(!hint.contains("first check"), "{hint}");
}

#[test]
fn every_function_names_its_own_answer() {
    let one = answer_with_for_request(&ModbusRequest::ReadInputRegisters {
        start: 7,
        quantity: 1,
    });
    assert!(
        one.starts_with(
            "send_modbus_registers with a registers object holding every input register"
        ),
        "{one}"
    );
    assert!(one.contains("asked for input register 7;"), "{one}");
    let bits = answer_with_for_request(&ModbusRequest::ReadCoils {
        start: 0,
        quantity: 8,
    });
    assert!(
        bits.starts_with("send_modbus_bits with a bits object holding every coil"),
        "{bits}"
    );
    assert!(bits.contains("coils 0 to 7"), "{bits}");
    let inputs = answer_with_for_request(&ModbusRequest::ReadDiscreteInputs {
        start: 3,
        quantity: 2,
    });
    assert!(inputs.contains("discrete inputs 3 to 4"), "{inputs}");
    // The top of the address space does not overflow the range arithmetic.
    let top = answer_with_for_request(&ModbusRequest::ReadHoldingRegisters {
        start: 65535,
        quantity: 1,
    });
    assert!(top.contains("holding register 65535;"), "{top}");
}

#[test]
fn a_write_leads_with_the_check_and_the_literal_exception() {
    let write = answer_with_for_request(&ModbusRequest::WriteSingleRegister {
        address: 10,
        value: 1,
    });
    assert!(
        write.starts_with(
            "first check whether your instructions give this device holding register 10."
        ),
        "{write}"
    );
    assert!(
        write.contains(r#"{"type": "send_modbus_exception", "exception_code": 2}"#),
        "{write}"
    );
    assert!(write.contains("send_modbus_write_ack"), "{write}");
    // The worked comparison is computed from the request, never from an instruction.
    assert!(
        write.contains("one whose holding registers are 0 to 9 has no holding register 10"),
        "{write}"
    );
    let at_zero = answer_with_for_request(&ModbusRequest::WriteMultipleCoils {
        start: 0,
        values: vec![true, false, true],
    });
    assert!(
        at_zero.contains("one whose coils start at 3 has no coil 0"),
        "{at_zero}"
    );
}
