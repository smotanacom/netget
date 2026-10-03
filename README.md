# NetGet

**LLM-Controlled Network Protocol Server & Client**

NetGet is a Rust CLI application exposing 192 network protocols as Cargo features, with server and client roles controlled by an LLM (via Ollama). Instead of hardcoding protocol logic, NetGet provides the network stack while the LLM constructs raw protocol datagrams or high-level responses based on natural language instructions.
