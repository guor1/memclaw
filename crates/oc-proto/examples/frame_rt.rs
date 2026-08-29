use oc_proto::*;
fn rt(f: &Frame) -> Frame {
    let s = serde_json::to_string(f).unwrap();
    println!("{s}");
    serde_json::from_str::<Frame>(&s).unwrap()
}
fn main() {
    // SessionsList 请求
    rt(&Frame::Req(Req{ id: ReqId::new("r1"), method: Method::SessionsList, idempotency_key: None }));
    // SessionReset 带 session
    rt(&Frame::Req(Req{ id: ReqId::new("r2"), method: Method::SessionReset(SessionResetParams{ session: Some(SessionId::new("work")) }), idempotency_key: None }));
    // Sessions 应答
    rt(&Frame::Res(Res{ id: ReqId::new("r1"), result: ResResult::Ok(MethodOk::Sessions(vec![SessionView{ id: SessionId::main(), kind:"main".into(), created_at:1, reset_at:0 }])) }));
    // 带 session 的 Assistant 事件
    rt(&Frame::Event(Event::Assistant{ session: SessionId::main(), run_id: RunId::new("run1"), delta: "hi".into() }));
    println!("PROTO_VERSION={}", PROTO_VERSION);
}
