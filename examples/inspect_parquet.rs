use parquet::file::reader::{FileReader, SerializedFileReader};
use std::fs::File;
use std::collections::HashMap;

fn main() {
    let path = "data/ledger.parquet";
    let file = File::open(path).expect("Failed to open file");
    let reader = SerializedFileReader::new(file).expect("Failed to create reader");

    let mut row_iter = reader.get_row_iter(None).expect("Failed to get row iter");
    let mut company_counts = HashMap::new();
    let mut total_rows = 0;

    while let Some(row_res) = row_iter.next() {
        let row = row_res.expect("Row error");
        total_rows += 1;
        for (name, field) in row.get_column_iter() {
            if name == "company" {
                if let parquet::record::Field::Str(val) = field {
                    *company_counts.entry(val.to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    println!("Total rows in parquet: {}", total_rows);
    println!("Unique companies and their row counts:");
    let mut counts: Vec<_> = company_counts.into_iter().collect();
    counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    for (co, count) in counts {
        println!("  Company {}: {}", co, count);
    }
}
