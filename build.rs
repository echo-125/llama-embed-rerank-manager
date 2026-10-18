fn main() {
    println!("cargo:rerun-if-changed=assets/llama.rc");
    println!("cargo:rerun-if-changed=assets/llama.ico");
    embed_resource::compile("assets/llama.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
