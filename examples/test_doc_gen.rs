use netget::llm::actions::common::generate_single_protocol_doc_data;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Test generating documentation for TCP protocol (simpler protocol)
    match generate_single_protocol_doc_data("tcp") {
        Ok(doc_data) => {
            println!("=== Protocol Documentation Data Structure ===\n");
            println!("Protocol Name: {}", doc_data.protocol_name);
            println!("Both Modes Available: {}", doc_data.both_modes);

            if let Some(server) = &doc_data.server {
                println!("\n=== Server Mode ===");
                println!("Stack Name: {}", server.stack_name);
                println!("Group: {}", server.group_name);
                println!("Description: {}", server.description);
                println!("Example Prompt: {}", server.example_prompt);
                println!("Keywords: {:?}", server.keywords);
                println!("Startup Params Count: {}", server.startup_params.len());
                println!("State: {}", server.state);
                if let Some(notes) = &server.notes {
                    println!("Notes: {}", notes);
                }
            }

            if let Some(client) = &doc_data.client {
                println!("\n=== Client Mode ===");
                println!("Stack Name: {}", client.stack_name);
                println!("Group: {}", client.group_name);
                println!("Description: {}", client.description);
                println!("Example Prompt: {}", client.example_prompt);
                println!("Keywords: {:?}", client.keywords);
                println!("Startup Params Count: {}", client.startup_params.len());
                println!("State: {}", client.state);
                if let Some(notes) = &client.notes {
                    println!("Notes: {}", notes);
                }
            }

            // Now render via template
            println!("\n\n=== Rendered Template Output ===\n");
            let template_engine = &netget::llm::template_engine::TEMPLATE_ENGINE;
            match template_engine.render(
                "shared/partials/base_stack_docs",
                &serde_json::json!({ "base_stack_docs": doc_data }),
            ) {
                Ok(rendered) => {
                    println!("{}", rendered);
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
        Err(e) => {
            return Err(e.into());
        }
    }
    Ok(())
}
