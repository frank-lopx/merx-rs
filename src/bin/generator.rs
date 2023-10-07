use std::io;
use std::io::Write;
use std::fs::File;
use std::path::Path;
use std::io::{BufRead, BufReader};
use anyhow::Result;
use merx::order::OrderRequest;
use merx::order::util::generate;

const FILE_PATH: &str = "./orders.json";

fn main() -> Result<()> {
    if !Path::new(FILE_PATH).exists() {
        generate_and_write_orders()?;
    }

    let mut stdout = io::stdout();

    let input = File::open(FILE_PATH)?;
    let reader = BufReader::new(input);

    let mut orders: Vec<OrderRequest> = Vec::new();

    for line in reader.lines() {
        let line = line?;

        let order: OrderRequest = serde_json::from_str(&line)?;

        orders.push(order);
    }
    
    for order in &orders {
        let order_json = serde_json::to_string(&order)?;

        writeln!(stdout, "{}", order_json)?;
    }

    Ok(())
}

fn generate_and_write_orders() -> io::Result<()> {
    let range = 1..=10_000_000;
    let mut file = File::create(FILE_PATH)?;

    for order in generate(range) {
        let order = serde_json::to_string(&order).ok().unwrap();
        writeln!(file, "{}", order)?;
    }

    Ok(())
}