//! The client half of the shared Modbus codec: `encode_request` and `parse_response`.
//!
//! No device and no model. Every check here is one a response must pass before the model is
//! shown it, and each was verified by removing it from `src/server/modbus/codec.rs` and watching
//! its assertion fail.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features modbus --test client -- modbus::codec_test --test-threads=100

use netget::client::modbus::actions::request_from_action;
use netget::server::modbus::codec::{
    encode_request, parse_response, ModbusRequest, ModbusResponse,
};
use serde_json::json;

#[test]
fn requests_encode_to_the_bytes_the_specification_gives() {
    assert_eq!(
        encode_request(&ModbusRequest::ReadHoldingRegisters {
            start: 0x006B,
            quantity: 3
        }),
        Ok(vec![0x03, 0x00, 0x6B, 0x00, 0x03])
    );
    assert_eq!(
        encode_request(&ModbusRequest::WriteSingleCoil {
            address: 0x00AC,
            value: true
        }),
        Ok(vec![0x05, 0x00, 0xAC, 0xFF, 0x00])
    );
    // The specification's own FC 15 example: ten coils from 0x13, packed CD 01.
    let bits = [
        true, false, true, true, false, false, true, true, true, false,
    ];
    assert_eq!(
        encode_request(&ModbusRequest::WriteMultipleCoils {
            start: 0x13,
            values: bits.to_vec()
        }),
        Ok(vec![0x0F, 0x00, 0x13, 0x00, 0x0A, 0x02, 0xCD, 0x01])
    );
    assert_eq!(
        encode_request(&ModbusRequest::WriteMultipleRegisters {
            start: 1,
            values: vec![0x000A, 0x0102]
        }),
        Ok(vec![
            0x10, 0x00, 0x01, 0x00, 0x02, 0x04, 0x00, 0x0A, 0x01, 0x02
        ])
    );
}

#[test]
fn a_request_the_specification_refuses_is_never_encoded() {
    for bad in [
        ModbusRequest::ReadHoldingRegisters {
            start: 0,
            quantity: 0,
        },
        ModbusRequest::ReadHoldingRegisters {
            start: 0,
            quantity: 126,
        },
        ModbusRequest::ReadCoils {
            start: 0,
            quantity: 2001,
        },
        ModbusRequest::ReadInputRegisters {
            start: 0xFFFF,
            quantity: 2,
        },
        ModbusRequest::WriteMultipleRegisters {
            start: 0,
            values: vec![0; 124],
        },
        ModbusRequest::WriteMultipleCoils {
            start: 0,
            values: vec![],
        },
    ] {
        assert!(encode_request(&bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn a_response_is_checked_against_its_request() {
    let read = ModbusRequest::ReadHoldingRegisters {
        start: 0,
        quantity: 2,
    };
    assert_eq!(
        parse_response(&[0x03, 0x04, 0x00, 0x0A, 0x01, 0x02], &read),
        Ok(ModbusResponse::Registers(vec![10, 258]))
    );
    // Wrong byte count for the quantity asked.
    assert!(parse_response(&[0x03, 0x02, 0x00, 0x0A], &read).is_err());
    // Byte count right, data short.
    assert!(parse_response(&[0x03, 0x04, 0x00, 0x0A], &read).is_err());
    // Another function's answer.
    assert!(parse_response(&[0x04, 0x04, 0x00, 0x0A, 0x01, 0x02], &read).is_err());
    // The exception form, and only in its exact two-octet shape.
    assert_eq!(
        parse_response(&[0x83, 0x02], &read),
        Ok(ModbusResponse::Exception { code: 2 })
    );
    assert!(parse_response(&[0x83, 0x02, 0x00], &read).is_err());

    // Bits: only the requested count, LSB first.
    let coils = ModbusRequest::ReadCoils {
        start: 0,
        quantity: 3,
    };
    assert_eq!(
        parse_response(&[0x01, 0x01, 0b1111_1101], &coils),
        Ok(ModbusResponse::Bits(vec![true, false, true]))
    );

    // A write acknowledgement must echo what was written.
    let write = ModbusRequest::WriteSingleRegister {
        address: 5,
        value: 1234,
    };
    assert_eq!(
        parse_response(&[0x06, 0x00, 0x05, 0x04, 0xD2], &write),
        Ok(ModbusResponse::WriteAck)
    );
    assert!(parse_response(&[0x06, 0x00, 0x05, 0x04, 0xD3], &write).is_err());
}

#[test]
fn model_numbers_are_refused_not_narrowed() {
    for bad in [
        json!({"type": "modbus_write_single_register", "address": 0, "value": 65536}),
        json!({"type": "modbus_write_single_register", "address": 70000, "value": 1}),
        json!({"type": "modbus_read_coils", "address": 0, "quantity": 2, "unit_id": 256}),
        json!({"type": "modbus_write_multiple_registers", "address": 0, "values": [1, -1]}),
        json!({"type": "modbus_write_single_coil", "address": 0, "value": 1}),
        json!({"type": "modbus_read_holding_registers", "address": 0, "quantity": 0}),
    ] {
        assert!(request_from_action(&bad).is_err(), "{bad} must be refused");
    }
}
