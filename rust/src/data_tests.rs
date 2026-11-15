//! SQLite fixture 回归：全部使用内存库或独立临时库，不触碰客户端数据。
use super::*;
use rusqlite::params;
use serde_json::json;

fn fixture()->Connection{
    let c=Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,title TEXT,directory TEXT,parent_id TEXT,time_created INTEGER,time_updated INTEGER,time_deleted INTEGER,revert TEXT,summary_additions INTEGER,summary_deletions INTEGER,summary_files INTEGER);
        CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);
        CREATE TABLE input_history(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER);
        CREATE TABLE model_usage(id TEXT PRIMARY KEY,session_id TEXT,turn_id TEXT,status TEXT,provider_id TEXT,model_id TEXT,agent TEXT,query_source TEXT,started_at INTEGER,completed_at INTEGER,first_token_at INTEGER,duration_ms INTEGER DEFAULT 0,time_to_first_token_ms INTEGER DEFAULT 0,input_tokens INTEGER DEFAULT 0,output_tokens INTEGER DEFAULT 0,reasoning_tokens INTEGER DEFAULT 0,cache_read_input_tokens INTEGER DEFAULT 0,cache_creation_input_tokens INTEGER DEFAULT 0,computed_total_tokens INTEGER DEFAULT 0,tool_call_count INTEGER DEFAULT 0,retry_count INTEGER DEFAULT 0,context_exceeded INTEGER DEFAULT 0);
        CREATE INDEX mu_session ON model_usage(session_id,turn_id);
        CREATE TABLE turn_usage(session_id TEXT,turn_id TEXT,status TEXT,started_at INTEGER,completed_at INTEGER,duration_ms INTEGER,time_to_first_token_ms INTEGER DEFAULT 0,tool_error_count INTEGER DEFAULT 0,cancelled_by_user INTEGER DEFAULT 0,model_request_count INTEGER DEFAULT 0,model_retry_count INTEGER DEFAULT 0,tool_call_count INTEGER DEFAULT 0,input_tokens INTEGER DEFAULT 0,output_tokens INTEGER DEFAULT 0,reasoning_tokens INTEGER DEFAULT 0,cache_read_input_tokens INTEGER DEFAULT 0,cache_creation_input_tokens INTEGER DEFAULT 0,computed_total_tokens INTEGER DEFAULT 0,PRIMARY KEY(session_id,turn_id));
        CREATE TABLE tool_usage(session_id TEXT,turn_id TEXT,tool_name TEXT,status TEXT,duration_ms INTEGER);
        CREATE TABLE part(session_id TEXT,data TEXT,time_created INTEGER,time_updated INTEGER);") .unwrap();c
}
fn session(c:&Connection,id:&str,parent:&str){c.execute("INSERT INTO session(id,title,directory,parent_id,time_created,time_updated) VALUES(?1,?1,'workspace',?2,100,100)",params![id,parent]).unwrap();}
fn message(c:&Connection,id:&str,sid:&str,t:i64,role:&str){c.execute("INSERT INTO message VALUES(?1,?2,?3,?4)",params![id,sid,t,json!({"role":role}).to_string()]).unwrap();}
fn request(c:&Connection,id:&str,sid:&str,turn:&str,status:&str,t:i64,input:i64,output:i64){
    c.execute("INSERT INTO model_usage(id,session_id,turn_id,status,provider_id,model_id,query_source,started_at,completed_at,first_token_at,duration_ms,time_to_first_token_ms,input_tokens,output_tokens,computed_total_tokens) VALUES(?1,?2,?3,?4,'fixture-provider','fixture-model',?5,?6,?7,?8,2000,1000,?9,?10,?11)",params![id,sid,turn,status,if sid.starts_with("sess_subagent"){"subagent"}else{"interactive"},t-2000,t,t-1000,input,output,input+output]).unwrap();
}
fn turn(c:&Connection,sid:&str,id:&str,status:&str,started:i64,completed:Option<i64>,cancelled:i64){
    c.execute("INSERT INTO turn_usage(session_id,turn_id,status,started_at,completed_at,duration_ms,model_request_count,tool_error_count,cancelled_by_user,input_tokens,output_tokens,computed_total_tokens) VALUES(?1,?2,?3,?4,?5,?6,4,2,?7,800,100,900)",params![sid,id,status,started,completed,completed.map(|t|t-started),cancelled]).unwrap();
}

#[test]fn empty_and_missing_tables_are_valid_snapshots(){
    let c=Connection::open_in_memory().unwrap();let s=snapshot(&c,0);
    assert!(s.ok,"{}",s.error);assert!(s.sid.is_empty());assert_eq!(s.context_window,0);
    let c=fixture();let s=snapshot(&c,0);assert!(s.ok);assert!(s.sid.is_empty());
    assert!(session_list(&c,20).unwrap().is_empty());assert_eq!(reports(&c,7).unwrap().daily.len(),7);
}
#[test]fn message_precedes_history_and_excludes_children_and_deleted(){
    let c=fixture();for id in ["composer","human","deleted"]{session(&c,id,"");}
    session(&c,"worker","human");session(&c,"sess_subagent_orphan","");
    c.execute("UPDATE session SET time_deleted=1 WHERE id='deleted'",[]).unwrap();
    message(&c,"u1","composer",1000,"user");message(&c,"u2","human",2000,"user");
    message(&c,"a1","composer",5000,"assistant");message(&c,"child","worker",9000,"user");
    message(&c,"dead","deleted",10000,"user");message(&c,"sub","sess_subagent_orphan",11000,"user");
    c.execute("INSERT INTO input_history VALUES('h','composer',20000)",[]).unwrap();
    let s=snapshot(&c,0);assert!(s.ok,"{}",s.error);assert_eq!(s.sid,"human");
}
#[test]fn input_history_fallback_has_stable_order_and_filters_parent(){
    let c=fixture();session(&c,"first","");session(&c,"second","");session(&c,"child","first");
    c.execute_batch("DROP TABLE message; INSERT INTO input_history VALUES('a','first',500),('z','second',500),('zz','child',1000);").unwrap();
    assert_eq!(snapshot(&c,0).sid,"second");
}
#[test]fn same_time_user_messages_use_id_tie_break(){
    let c=fixture();session(&c,"first","");session(&c,"second","");message(&c,"a","first",500,"user");message(&c,"z","second",500,"user");
    assert_eq!(snapshot(&c,0).sid,"second");
}
#[test]fn fallback_usage_excludes_parent_and_prefix_children(){
    let c=fixture();session(&c,"root","");session(&c,"child","root");session(&c,"sess_subagent_x","");
    request(&c,"r","root","t","completed",10000,100,10);request(&c,"c","child","t","completed",20000,200,20);request(&c,"p","sess_subagent_x","t","completed",30000,300,30);
    assert_eq!(snapshot(&c,0).sid,"root");
}
#[test]fn pin_new_session_and_pin_lost_fall_back_to_current(){
    let c=fixture();session(&c,"live","");session(&c,"new","");message(&c,"m","live",1000,"user");
    let s=snapshot_for(&c,0,"new");assert!(s.ok);assert_eq!(s.sid,"new");assert!(!s.pin_lost);assert_eq!(s.requests,0);
    let s=snapshot_for(&c,0,"missing");assert_eq!(s.sid,"live");assert!(s.pin_lost);assert_eq!(s.pinned,"missing");
    c.execute("UPDATE session SET time_deleted=2 WHERE id='new'",[]).unwrap();
    assert!(snapshot_for(&c,0,"new").pin_lost);
    assert!(!snapshot_for(&c,0,"bad;select").pin_lost);
}
#[test]fn compressed_context_uses_latest_input_and_tps_uses_its_own_completed_at(){
    let c=fixture();session(&c,"s","");
    request(&c,"old","s","t1","completed",10000,180000,500);
    request(&c,"recent","s","t2","completed",20000,20000,100);
    request(&c,"failed","s","t3","error",40000,999999,0);
    let s=snapshot_for(&c,0,"s");assert_eq!(s.ctx,20000);assert_eq!(s.input,200000);assert_eq!(s.last.tps,100.0);assert_eq!(s.last.completed_at,20000);assert_eq!(s.requests,2);
}
#[test]fn same_time_completed_requests_use_id_tie_break(){
    let c=fixture();session(&c,"s","");request(&c,"a","s","t","completed",20000,100000,10);request(&c,"z","s","t","completed",20000,10000,10);
    assert_eq!(snapshot_for(&c,0,"s").ctx,10000);
}
#[test]fn cancelled_or_failed_latest_turn_does_not_resurrect_success(){
    for status in ["error","cancelled"]{
        let c=fixture();session(&c,"s","");request(&c,"r","s","old","completed",20000,100,10);
        turn(&c,"s","old","completed",10000,Some(20000),0);turn(&c,"s","new",status,30000,Some(40000),0);
        let s=snapshot_for(&c,0,"s");assert!(s.turn_state.available);assert_eq!(s.turn_state.turn_id,"new");assert_eq!(s.turn_state.status,status);assert_eq!(s.last_turn.total,900);assert_eq!(s.last_turn.requests,4);assert_eq!(s.requests,1);assert!(!s.generating);
    }
}
#[test]fn cancellation_flag_overrides_completed_and_revert_disables_signal(){
    let c=fixture();session(&c,"s","");turn(&c,"s","t","completed",100,Some(1000),1);
    assert_eq!(snapshot_for(&c,0,"s").turn_state.status,"cancelled");
    c.execute("UPDATE session SET revert='{}' WHERE id='s'",[]).unwrap();assert!(!snapshot_for(&c,0,"s").turn_state.available);
}
#[test]fn malformed_terminal_timestamp_is_not_available(){
    let c=fixture();session(&c,"s","");turn(&c,"s","t","completed",2000,Some(1000),0);
    assert!(!snapshot_for(&c,0,"s").turn_state.available);
}
#[test]fn model_completed_is_not_a_turn_completion_signal(){
    let c=fixture();session(&c,"s","");request(&c,"r","s","t","completed",10000,100,10);
    c.execute_batch("DROP TABLE turn_usage;").unwrap();let s=snapshot_for(&c,0,"s");
    assert!(!s.turn_state.available);assert_eq!(s.last_turn.total,110);assert_eq!(s.last_turn.requests,1);
}
#[test]fn new_user_message_does_not_repeat_previous_turn_terminal(){
    let c=fixture();let now=now_ms();session(&c,"s","");turn(&c,"s","old","completed",now-10000,Some(now-5000),0);
    message(&c,"new","s",now-1000,"user");let s=snapshot_for(&c,0,"s");assert!(s.generating);assert!(s.active);assert!(!s.turn_state.available);assert_eq!(s.last_turn.total,0);
}
#[test]fn running_turn_is_generating_and_uses_live_request_totals(){
    let c=fixture();let now=now_ms();session(&c,"s","");turn(&c,"s","t","running",now-20000,None,0);
    request(&c,"r","s","t","completed",now-1000,100,20);let s=snapshot_for(&c,0,"s");
    assert!(s.generating);assert!(s.turn_state.available);assert_eq!(s.turn_state.status,"running");assert_eq!(s.last_turn.total,120);assert!(s.last_turn.duration_ms>=20000);
}
#[test]fn cache_rate_uses_input_without_double_counting(){
    let c=fixture();session(&c,"s","");request(&c,"r","s","t","completed",10000,1000,20);
    c.execute("UPDATE model_usage SET cache_read_input_tokens=800,cache_creation_input_tokens=100",[]).unwrap();
    let s=snapshot_for(&c,0,"s");assert_eq!(s.cache_rate,80.0);assert_eq!(s.total,1020);assert!(s.detail_text.contains("80.0%"));assert!(s.detail_text.contains("输入已含缓存"));assert_eq!(cache_rate(0,10),0.0);
}
#[test]fn context_error_survives_until_later_success(){
    let c=fixture();session(&c,"s","");request(&c,"ok","s","t1","completed",10000,100,10);request(&c,"bad","s","t2","error",20000,999,0);
    c.execute("UPDATE model_usage SET context_exceeded=1 WHERE id='bad'",[]).unwrap();let s=snapshot_for(&c,0,"s");assert!(s.context_exceeded);assert_eq!(s.ctx_exc,20000);
    request(&c,"ok2","s","t3","completed",30000,50,10);assert!(!snapshot_for(&c,0,"s").context_exceeded);
}
#[test]fn tools_include_errors_exclude_running_and_code_keeps_nulls(){
    let c=fixture();session(&c,"s","");
    c.execute_batch("INSERT INTO tool_usage VALUES('s','t','Read','completed',10),('s','t','Read','error',20),('s','t','Bash','running',30); UPDATE session SET summary_additions=3,summary_deletions=1,summary_files=2;").unwrap();
    let s=snapshot_for(&c,0,"s");assert_eq!(s.tools_total,2);assert_eq!(s.tools_errors,1);assert_eq!(s.tools.list[0].duration_ms,30);assert_eq!(s.code.add,Some(3));assert!(s.detail_text.contains("代码改动"));
}
#[test]fn subagent_mapping_uses_receipt_and_unique_prompt_not_time(){
    let c=fixture();session(&c,"root","");session(&c,"sess_subagent_agent_one","root");session(&c,"sess_subagent_agent_two","root");
    c.execute("UPDATE session SET title='Unique prompt for child...' WHERE id='sess_subagent_agent_two'",[]).unwrap();
    let p1=json!({"tool":"Agent","state":{"input":{"description":"receipt task","prompt":"unrelated"},"output":"done\nagentId: agent_one (resume)"}});
    let p2=json!({"tool":"Agent","state":{"input":{"description":"running task","prompt":"Unique prompt for child with details"},"output":""}});
    for p in [p1,p2]{c.execute("INSERT INTO part VALUES('root',?1,100,100)",[p.to_string()]).unwrap();}
    request(&c,"r","sess_subagent_agent_one","t","completed",10000,500,20);
    let s=snapshot_for(&c,0,"root");assert_eq!(s.sub.list.len(),2);assert_eq!(s.sub.total,520);
    assert_eq!(s.sub.list.iter().find(|v|v.sid.ends_with("one")).unwrap().task,"receipt task");
    assert_eq!(s.sub.list.iter().find(|v|v.sid.ends_with("two")).unwrap().task,"running task");
}
#[test]fn reports_use_non_overlapping_local_days_and_include_cache(){
    let c=fixture();session(&c,"s","");let (start,_)=day_bounds(1);
    request(&c,"yesterday","s","t1","completed",start-1,100,10);request(&c,"midnight","s","t2","completed",start,200,20);
    request(&c,"error","s","t3","error",start+100,999,0);
    c.execute("UPDATE model_usage SET cache_read_input_tokens=80,reasoning_tokens=2,retry_count=1",[]).unwrap();
    let r=reports(&c,7).unwrap();assert_eq!(r.today.total,220);assert_eq!(r.week.total,330);assert_eq!(r.daily.last().unwrap().usage.requests,1);assert_eq!(r.models[0].usage.total,330);assert_eq!(r.today.cache_read,80);
    let sessions=session_list(&c,20).unwrap();assert_eq!(sessions[0].requests,2);assert_eq!(sessions[0].total,330);
}
#[test]fn old_schema_with_missing_optional_columns_still_works(){
    let c=Connection::open_in_memory().unwrap();c.execute_batch("CREATE TABLE session(id TEXT,title TEXT);INSERT INTO session VALUES('s','old');CREATE TABLE model_usage(session_id TEXT,status TEXT,input_tokens INTEGER,completed_at INTEGER);INSERT INTO model_usage VALUES('s','completed',100,1000);").unwrap();
    let s=snapshot_for(&c,0,"s");assert!(s.ok,"{}",s.error);assert_eq!(s.input,100);assert_eq!(s.ctx,100);assert!(!s.turn_state.available);
}
#[test]fn session_list_includes_usage_without_session_metadata(){
    let c=fixture();session(&c,"s","");request(&c,"r","orphan","t","completed",10000,100,10);
    let rows=session_list(&c,10).unwrap();assert!(rows.iter().any(|s|s.sid=="orphan"&&s.total==110));
    assert_eq!(snapshot_for(&c,0,"orphan").total,110);
}
#[test]fn malformed_message_json_does_not_break_current_session(){
    let c=fixture();session(&c,"s","");message(&c,"good","s",1000,"user");
    c.execute("INSERT INTO message VALUES('bad','s',2000,'{broken')",[]).unwrap();
    let s=snapshot(&c,0);assert!(s.ok,"{}",s.error);assert_eq!(s.sid,"s");
}
#[test]fn readonly_connections_allow_empty_database_and_reject_writes(){
    let dir=std::env::temp_dir().join(format!("zcode-data-readonly-{}-{}",std::process::id(),now_ms()));std::fs::create_dir_all(&dir).unwrap();let path=dir.join("fixture.sqlite");
    {let c=Connection::open(&path).unwrap();c.execute_batch("CREATE TABLE empty(id INTEGER);").unwrap();}
    let c=connect_readonly_at(&path).unwrap();assert_eq!(c.query_row("PRAGMA query_only",[],|r|r.get::<_,i64>(0)).unwrap(),1);assert_eq!(c.query_row("PRAGMA busy_timeout",[],|r|r.get::<_,i64>(0)).unwrap(),3000);
    assert!(c.execute("INSERT INTO empty VALUES(1)",[]).is_err());assert!(snapshot(&c,0).ok);drop(c);
    let missing=dir.join("not-created.sqlite");assert!(connect_readonly_at(&missing).is_err());assert!(!missing.exists());std::fs::remove_dir_all(&dir).unwrap();
}
#[test]fn subagent_turn_is_not_a_parent_completion_event(){
    let c=fixture();session(&c,"root","");session(&c,"worker","root");turn(&c,"worker","t","completed",100,Some(1000),0);
    let s=snapshot_for(&c,0,"worker");assert!(s.is_sub);assert!(s.turn_state.is_sub);assert!(!s.turn_state.available);
}
#[test]fn terminal_turn_overrides_a_stale_running_model_request(){
    let c=fixture();let now=now_ms();session(&c,"s","");request(&c,"stale","s","t","running",now-1000,0,0);
    turn(&c,"s","t","cancelled",now-10000,Some(now-500),1);
    let s=snapshot_for(&c,0,"s");assert_eq!(s.turn_state.status,"cancelled");assert!(!s.generating);
}
#[test]fn aggregate_cache_is_database_scoped_and_invalidates_on_writes(){
    let dir=std::env::temp_dir().join(format!("zcode-data-cache-{}-{}",std::process::id(),now_ms()));std::fs::create_dir_all(&dir).unwrap();
    let path=dir.join("fixture.sqlite");let writer=Connection::open(&path).unwrap();writer.execute_batch("CREATE TABLE values_for_test(v INTEGER);INSERT INTO values_for_test VALUES(1);").unwrap();
    let con=connect_readonly_at(&path).unwrap();let count=std::cell::Cell::new(0);
    let query=||{count.set(count.get()+1);con.query_row("SELECT sum(v) FROM values_for_test",[],|r|r.get::<_,i64>(0))};
    assert_eq!(cached(&con,"fixture-cache",Duration::ZERO,query).unwrap(),1);
    assert_eq!(cached(&con,"fixture-cache",Duration::ZERO,query).unwrap(),1);assert_eq!(count.get(),1);
    writer.execute("INSERT INTO values_for_test VALUES(2)",[]).unwrap();
    assert_eq!(cached(&con,"fixture-cache",Duration::ZERO,query).unwrap(),3);assert_eq!(count.get(),2);
    writer.execute("INSERT INTO values_for_test VALUES(4)",[]).unwrap();
    assert_eq!(cached(&con,"fixture-cache",Duration::from_secs(8),query).unwrap(),3);assert_eq!(count.get(),2);
    let other=Connection::open(dir.join("other.sqlite")).unwrap();
    assert_eq!(cached(&other,"fixture-cache",Duration::from_secs(8),||Ok(99i64)).unwrap(),99);
    drop(other);drop(con);drop(writer);std::fs::remove_dir_all(dir).unwrap();
}
#[test]fn reader_recovers_after_database_appears(){
    let dir=std::env::temp_dir().join(format!("zcode-data-recover-{}-{}",std::process::id(),now_ms()));std::fs::create_dir_all(&dir).unwrap();let path=dir.join("fixture.sqlite");let mut reader=DataReader::with_path(&path);
    assert!(!reader.snapshot(0,"").ok);
    {let c=Connection::open(&path).unwrap();c.execute_batch("CREATE TABLE session(id TEXT,title TEXT);INSERT INTO session VALUES('new','created');").unwrap();}
    let s=reader.snapshot(0,"");assert!(s.ok,"{}",s.error);assert_eq!(s.sid,"new");drop(reader);std::fs::remove_dir_all(&dir).unwrap();
}
