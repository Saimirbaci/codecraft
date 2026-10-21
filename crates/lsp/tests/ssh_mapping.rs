//! A fake ssh executable is the server transport; no sockets, host, credentials or user home.
#![cfg(unix)]
use codecraft_lsp::{LspClient, ServerConfig, ServerEvent, path_mapper::RootMapper};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[test]
fn ssh_stdio_maps_initialize_open_definition_diagnostics_and_rename() {
    use std::os::unix::fs::PermissionsExt;
    let Some(python) = codecraft_lsp::which("python3") else { return };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("naïve #项目");
    std::fs::create_dir_all(&root).unwrap();
    let ssh = dir.path().join("ssh");
    let log = dir.path().join("messages.jsonl");
    let mock = r#"
import sys, json, os
from urllib.parse import quote
uri='file:///srv/'+quote('项目 space')+'/na%C3%AFve%20%23.rs'
def send(v):
 b=json.dumps(v,ensure_ascii=False).encode();sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(b)).encode()+b);sys.stdout.buffer.flush()
while True:
 headers={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line==b'\r\n': break
  k,v=line.decode().split(':',1);headers[k.lower()]=v.strip()
 msg=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
 with open(os.environ['LOG'],'a') as f: f.write(json.dumps(msg)+'\n')
 method=msg.get('method')
 if method=='initialize': result={'capabilities':{'definitionProvider':True,'renameProvider':True}}
 elif method=='textDocument/didOpen':
  send({'jsonrpc':'2.0','method':'textDocument/publishDiagnostics','params':{'uri':uri,'diagnostics':[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}},'message':'fixture'}]}});continue
 elif method=='textDocument/definition': result=[{'uri':uri,'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}}}]
 elif method=='textDocument/rename': result={'changes':{uri:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':1}},'newText':msg['params']['newName']}]}}
 elif method=='shutdown': result=None
 elif method=='exit': sys.exit(0)
 else: continue
 send({'jsonrpc':'2.0','id':msg['id'],'result':result})
"#;
    std::fs::write(&ssh, format!("#!{}\n{mock}", python.display())).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = ServerConfig::rust_analyzer(ssh, root.clone());
    config.args = vec!["-T".into(), "fixture.invalid".into()];
    config.env = vec![("HOME".into(), dir.path().display().to_string()), ("LOG".into(), log.display().to_string())];
    let mapper = RootMapper::new(&root.to_string_lossy(), "/srv/项目 space");
    let (client, events) = LspClient::spawn_mapped(config, Arc::new(mapper)).unwrap();
    assert!(matches!(events.recv_timeout(Duration::from_secs(5)).unwrap(), ServerEvent::Initialized(_)));
    let path = root.join("naïve #.rs");
    client.did_open(&path, "rust", 1, "fn main() {} // /srv/项目 space").unwrap();
    let diagnostic = events.recv_timeout(Duration::from_secs(5)).unwrap();
    let ServerEvent::Diagnostics(d) = diagnostic else { panic!("{diagnostic:?}") };
    assert_eq!(codecraft_lsp::uri_to_path(&d.uri), Some(path.clone()));
    client.definition(&path, 0, 0).unwrap();
    let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
    let ServerEvent::Response { result: Ok(value), .. } = event else { panic!("{event:?}") };
    assert_eq!(value[0]["uri"], json!(codecraft_lsp::path_to_uri(&path).unwrap()));
    client.rename(&path, 0, 0, "/srv/项目 space/literal").unwrap();
    let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
    let ServerEvent::Response { result: Ok(value), .. } = event else { panic!("{event:?}") };
    let uri = codecraft_lsp::path_to_uri(&path).unwrap();
    assert_eq!(value["changes"][uri.as_str()][0]["newText"], "/srv/项目 space/literal");
    client.shutdown(Duration::from_secs(1)).unwrap();
    let recorded: Vec<Value> =
        std::fs::read_to_string(log).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect();
    let init = recorded.iter().find(|m| m["method"] == "initialize").unwrap();
    assert!(init["params"]["processId"].is_null());
    assert_eq!(init["params"]["rootUri"], "file:///srv/%E9%A1%B9%E7%9B%AE%20space");
    let open = recorded.iter().find(|m| m["method"] == "textDocument/didOpen").unwrap();
    assert_eq!(open["params"]["textDocument"]["uri"], "file:///srv/%E9%A1%B9%E7%9B%AE%20space/na%C3%AFve%20%23.rs");
    assert_eq!(open["params"]["textDocument"]["text"], "fn main() {} // /srv/项目 space");
    let rename = recorded.iter().find(|m| m["method"] == "textDocument/rename").unwrap();
    assert_eq!(rename["params"]["textDocument"]["uri"], open["params"]["textDocument"]["uri"]);
    let _: PathBuf = root;
}
