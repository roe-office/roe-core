fn main() {
    let samples = vec![
        "=[.A112]=[.B112]",
        "=ROUND([.A2];12)=ROUND([.B2];12)",
        "=MATCH([.G82];[.G82:.G86];0)",
    ];
    for s in samples {
        match formualizer_parse::parse_with_dialect(s, formualizer_parse::FormulaDialect::OpenFormula) {
            Ok(ast) => println!("OK: {} -> {:?}", s, ast),
            Err(e) => println!("ERR: {} -> {:?}", s, e),
        }
    }
}
