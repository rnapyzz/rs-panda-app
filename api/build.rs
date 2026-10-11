// migrations/*.sql をバイナリに埋め込む。
// ファイル名から拡張子を除いたものをバージョンとし、名前の順に並べる。
use std::{env, fs, path::Path};

fn main() {
    // パスはビルドスクリプトの実行時に読む（コンパイル時に埋め込むと、ディレクトリを移したときに古いパスを見る）
    let dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("migrations");
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("read migrations dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    names.sort();

    let mut out = String::from("pub static MIGRATIONS: &[(&str, &str)] = &[\n");
    for n in &names {
        let path = dir.join(n);
        println!("cargo:rerun-if-changed={}", path.display());
        out.push_str(&format!(
            "    ({:?}, include_str!({:?})),\n",
            n.trim_end_matches(".sql"),
            path.display().to_string()
        ));
    }
    out.push_str("];\n");
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("migrations.rs"), out).unwrap();
}
