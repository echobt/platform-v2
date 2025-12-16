fn main() {
    let secret_hex = "0000000000000000000000000000000000000000000000000000000000000001";
    let bytes = hex::decode(secret_hex).expect("Invalid hex");
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);

    let signing_key = ed25519_dalek::SigningKey::from_bytes(&arr);
    let verifying_key = signing_key.verifying_key();
    println!("{}", hex::encode(verifying_key.to_bytes()));
}
