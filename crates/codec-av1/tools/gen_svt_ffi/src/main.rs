fn main() {
    let h = std::env::args().nth(1).unwrap();
    let b = bindgen::Builder::default()
        .header(format!("{h}/EbSvtAv1Enc.h"))
        .clang_arg(format!("-I{h}"))
        .allowlist_function("svt_av1_(enc_.*|get_version)")
        .allowlist_type("EbSvtAv1EncConfiguration|EbBufferHeaderType|EbComponentType|EbErrorType|EbAv1PictureType|EbSvtIOFormat|SvtAv1IntraRefreshType|SvtAv1PredStructure|SvtAv1RcMode|EbColorFormat")
        .allowlist_var("EB_BUFFERFLAG_.*|SVT_AV1_.*")
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .layout_tests(true)
        .dynamic_library_name("SvtAv1Lib")
        .dynamic_link_require_all(true)
        .derive_default(true)
        .derive_debug(false)
        .generate_comments(false)
        .use_core()
        .generate().expect("bindgen");
    println!("{}", b.to_string());
}
