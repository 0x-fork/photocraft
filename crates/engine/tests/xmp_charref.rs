use photocraft_engine::file_cmds::read_file_info;

#[test]
fn numeric_character_references_are_decoded() {
    let xmp = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF><rdf:Description>\
        <dc:description><rdf:Alt><rdf:li xml:lang=\"x-default\">Line one&#xA;Line two &#169; &#38; co &#x41; &amp;#38;</rdf:li></rdf:Alt></dc:description>\
        </rdf:Description></rdf:RDF></x:xmpmeta>";
    let info = read_file_info(Some(xmp));
    assert_eq!(info["description"], "Line one\nLine two © & co A &#38;");
}
