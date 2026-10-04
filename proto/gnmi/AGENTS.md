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
both roles plus an immutable descriptor set. No additional protobuf runtime family is introduced.

The three Google well-known types gnmi.proto imports are vendored unchanged from
protocolbuffers/protobuf v21.12 (BSD-3-Clause, `google/protobuf/LICENSE`) so the build does not
depend on how a platform packages protoc's includes: Ubuntu's `protobuf-compiler` ships without
them (they live in `libprotobuf-dev`), which failed the first CI build. v21.12 matches Ubuntu
24.04's protoc 3.21.12 and parses on every newer protoc. prost still maps these types to
`prost-types`; the files only resolve imports.

- google/protobuf/any.proto SHA256:
  1aa80cf90ddbd380b73b1422bae51d76d99b82a4cdf0b183f21a802f72aafe8b
- google/protobuf/descriptor.proto SHA256:
  7b393792dec5a4931926fe6ac62b1939365572e9dc498232d267e9b7285818a9
- google/protobuf/duration.proto SHA256:
  099047097e8fe73657b49ef67af914a7a686ac6154f9d872882708b5eb3db04c
