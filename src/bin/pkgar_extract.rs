fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: pkgar_extract <pkey.toml|-> <archive.pkgar> <destination>");
        std::process::exit(2);
    }
    let pkey_path = if args[1] == "-" {
        // Source packages embed their signing key in the archive header.
        let pkey = match pkgar_keys::PublicKeyFile::from_header_path(&args[2]) {
            Ok(pkey) => pkey,
            Err(err) => {
                eprintln!("failed to read the embedded public key: {err:?}");
                std::process::exit(1);
            }
        };
        let path = "/tmp/pkgar-source-pubkey.toml";
        if let Err(err) = pkey.save(path) {
            eprintln!("failed to write the public key: {err:?}");
            std::process::exit(1);
        }
        path.to_string()
    } else {
        args[1].clone()
    };
    if let Err(err) = pkgar::extract(&pkey_path, &args[2], &args[3]) {
        eprintln!("pkgar extract failed: {err:?}");
        std::process::exit(1);
    }
}
