fn main() {
    println!("cargo:rerun-if-changed=assets/kadr.ico");
    println!("cargo:rerun-if-changed=assets/kadr.manifest");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/kadr.ico");
    res.set_manifest_file("assets/kadr.manifest");
    res.set("FileDescription", "Kadr");
    res.set("ProductName", "Kadr");
    res.compile().expect("failed to compile Windows resources");
}
