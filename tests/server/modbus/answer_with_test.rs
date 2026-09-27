//! The `answer_with` hint each Modbus event carries.
//!
//! Told "ten holding registers at addresses 0 to 9 ... nothing at any other address",
//! llama3.1:8b answered a read of 500-501 with `[0, 0]` five runs in five: it does not compare a
//! start address and a quantity with a range given in prose. The hint names the exact addresses,
//! leads with the check, and gives exception 2 as the literal action. `e2e_test.rs` proves the
//! field reaches the event.

use netget::server::modbus::actions::answer_with_for_request;
use netget::server::modbus::codec::ModbusRequest;

#[test]
fn a_register_read_names_its_addresses_and_leads_with_exception_2() {
    let hint = answer_with_for_request(&ModbusRequest::ReadHoldingRegisters {
        start: 500,
        quantity: 2,
    });
    assert!(
        hint.starts_with(
            "first check whether your instructions give this device holding registers 500 to 501."
        ),
        "{hint}"
    );
    assert!(
        hint.contains(r#"{"type": "send_modbus_exception", "exception_code": 2}"#),
        "{hint}"
    );
    assert!(
        hint.contains("send_modbus_registers with exactly 2 numbers"),
        "{hint}"
    );
    // The worked comparison is computed from the request, never from an instruction.
    assert!(
        hint.contains("one whose holding registers are 0 to 499 has no holding register 500"),
        "{hint}"
    );
    let at_zero = answer_with_for_request(&ModbusRequest::ReadHoldingRegisters {
        start: 0,
        quantity: 3,
    });
    assert!(
        at_zero.contains("one whose holding registers start at 3 has no holding register 0"),
        "{at_zero}"
    );
    // The exception comes before the values: worded values-first, the model answered with
    // prose or zeros.
    assert!(
        hint.find("send_modbus_exception") < hint.find("send_modbus_registers"),
        "{hint}"
    );
}

#[test]
fn every_function_names_its_own_answer() {
    let one = answer_with_for_request(&ModbusRequest::ReadInputRegisters {
        start: 7,
        quantity: 1,
    });
    assert!(one.contains("give this device input register 7."), "{one}");
    let bits = answer_with_for_request(&ModbusRequest::ReadCoils {
        start: 0,
        quantity: 8,
    });
    assert!(
        bits.contains("send_modbus_bits with exactly 8 booleans"),
        "{bits}"
    );
    assert!(bits.contains("coils 0 to 7"), "{bits}");
    let inputs = answer_with_for_request(&ModbusRequest::ReadDiscreteInputs {
        start: 3,
        quantity: 2,
    });
    assert!(inputs.contains("discrete inputs 3 to 4"), "{inputs}");
    let write = answer_with_for_request(&ModbusRequest::WriteSingleRegister {
        address: 10,
        value: 1,
    });
    assert!(write.contains("send_modbus_write_ack"), "{write}");
    assert!(write.contains("holding register 10."), "{write}");
    // The top of the address space does not overflow the range arithmetic.
    let top = answer_with_for_request(&ModbusRequest::ReadHoldingRegisters {
        start: 65535,
        quantity: 1,
    });
    assert!(top.contains("holding register 65535."), "{top}");
}
