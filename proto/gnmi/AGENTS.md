# Pinned OpenConfig gNMI protobufs

These files are copied unchanged from openconfig/gnmi v0.14.1, preserving original
Google copyright and Apache-2.0 headers and import paths. The service advertises gNMI
0.10.0. They define wire types, not a claim that every encoding/extension is implemented.

- github.com/openconfig/gnmi/proto/gnmi/gnmi.proto SHA256:
  45abf90bfee289544e2430c8ce1b5b10e4f851736a508ddba54d3cf12e82f7b7
- github.com/openconfig/gnmi/proto/gnmi_ext/gnmi_ext.proto SHA256:
  b0e96bd0c540cf249512783ee5abeb3f0d05805115f18c7d714ac90316981e2e

Primary source: https://github.com/openconfig/gnmi/tree/v0.14.1/proto.
build.rs invokes existing tonic-build0.12/prost-build0.13 only for feature gnmi and emits
both roles plus an immutable descriptor set. protoc's standard includes provide Google
well-known types. No additional protobuf runtime family is introduced.
