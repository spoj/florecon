use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use std::fs::File;

fn main() {
    let path = "data/ledger.parquet";
    let file = File::open(path).expect("Failed to open file");
    let reader = SerializedFileReader::new(file).expect("Failed to create reader");

    let mut row_iter = reader.get_row_iter(None).expect("Failed to get row iter");
    if let Some(row_res) = row_iter.next() {
        let row = row_res.expect("Row error");
        for (name, field) in row.get_column_iter() {
            if name == "as_of_date" {
                if let Field::Date(val) = field {
                    let val_typed: i32 = *val; // verify it is i32
                    println!("as_of_date is a u32: {}", val_typed);
                }
            }
        }
    }
}
