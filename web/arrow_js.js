import { RecordBatchStreamWriter, RecordBatch, makeVector, Uint64, Int64, Utf8, Field, Schema as ArrowSchema } from "apache-arrow";

export function rowsToArrow(int_cols, token_cols, ids, int_data, token_data) {
    const fields = [new Field("id", new Uint64(), false)];
    for (const name of int_cols) {
        fields.push(new Field(name, new Int64(), true));
    }
    for (const name of token_cols) {
        fields.push(new Field(name, new Utf8(), true));
    }
    const schema = new ArrowSchema(fields);

    const children = [];
    // ids is an array of numbers (or bigints)
    children.push(makeVector({ data: ids, type: new Uint64() }));

    for (let i = 0; i < int_cols.length; i++) {
        children.push(makeVector({ data: int_data[i], type: new Int64() }));
    }
    for (let i = 0; i < token_cols.length; i++) {
        children.push(makeVector({ data: token_data[i], type: new Utf8() }));
    }

    const batch = new RecordBatch(schema, { numRows: ids.length, children });
    const writer = RecordBatchStreamWriter.writeAll([batch]);
    return writer.toUint8Array();
}
