use mb_client::model;

const STUDIES_CSV: &str = include_str!("fixtures/studies.csv");

#[test]
fn test_studies_csv() {
    let studies: Vec<model::Study> = csv::Reader::from_reader(STUDIES_CSV.as_bytes())
        .deserialize()
        .collect::<Result<_, _>>()
        .expect("documented study CSV should deserialize");

    assert_eq!(studies.len(), 1);
    let study = &studies[0];
    assert_eq!(study.id, 2_911_040);
    assert_eq!(study.name, "Galapagos Albatrosses");
    assert!(study.i_have_download_access);
}
