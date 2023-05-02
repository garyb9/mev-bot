
const FLASHBOTS_BUNLDE_RELAY_URL: &str = "https://relay-goerli.flashbots.net";

fn main() {
    dotenv::dotenv().ok();
    println!("{}", FLASHBOTS_BUNLDE_RELAY_URL);
}
