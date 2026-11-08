"""Real app-server resume smoke; synthetic history, no credentials or inference."""
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import uuid

root = Path(__file__).resolve().parents[1]
adapter = root / 'target/release/codex-shared-resume.exe'
real = Path(os.environ['SHARED_RESUME_CODEX'])

def run(home, path, provider, model):
    env = dict(os.environ, CODEX_HOME=str(home), AGENTCOCKPIT_CODEX_REAL=str(real))
    for key in ['OPENAI_API_KEY', 'OPENAI_BASE_URL', 'OPENAI_ACCESS_TOKEN']:
        env.pop(key, None)
    p = subprocess.Popen([str(adapter), 'app-server'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True, encoding='utf-8', env=env)
    messages = queue.Queue()
    def read():
        for line in p.stdout:
            try: messages.put(json.loads(line))
            except ValueError: pass
    threading.Thread(target=read, daemon=True).start()
    def call(id, method, params):
        p.stdin.write(json.dumps(dict(id=id, method=method, params=params))+'\n'); p.stdin.flush()
        import time
        deadline=time.monotonic()+40
        while time.monotonic()<deadline:
            msg=messages.get(timeout=max(.1,deadline-time.monotonic()))
            if msg.get('id') == id:
                assert 'error' not in msg, msg.get('error')
                return msg['result']
        raise TimeoutError(method)
    try:
        call(1,'initialize',{'clientInfo':{'name':'resume_smoke','version':'1.0'},'capabilities':{'experimentalApi':True}})
        p.stdin.write('{"method":"initialized"}\n');p.stdin.flush()
        result=call(2,'thread/resume',{'threadId':thread_id,'path':str(path),'model':None,'modelProvider':None})
        assert result['modelProvider']==provider, result.get('modelProvider')
        assert result['model']==model, result.get('model')
        assert result['thread']['id']==thread_id
        assert 'shared smoke context' in json.dumps(result), 'History content was not restored'
        print('PASS:',provider,model,'same thread and history restored')
    finally:
        p.stdin.close()
        try:p.wait(timeout=8)
        except subprocess.TimeoutExpired:p.kill();p.wait()

with tempfile.TemporaryDirectory(prefix='shared-resume-smoke-') as directory:
    base=Path(directory)
    thread_id=str(uuid.uuid4())
    fixture=base/'rollout.jsonl'
    records=[
        {'type':'session_meta','payload':{'id':thread_id,'timestamp':'2026-11-01T00:00:00Z','cwd':str(base),'originator':'codex_cli_rs','cli_version':'0.161.0','source':'cli','model_provider':'missing-old-provider'}},
        {'type':'response_item','payload':{'type':'message','role':'user','content':[{'type':'input_text','text':'shared smoke context'}]}},
        {'type':'event_msg','payload':{'type':'user_message','message':'shared smoke context','images':[]}},
    ]
    for provider,model in [('openai','gpt-6.1-terra'),('agentcockpit','test-api-model')]:
        fixture.write_text(''.join(json.dumps(dict(timestamp='2026-11-01T00:00:00Z',**r))+'\n' for r in records),encoding='utf-8')
        home=base/provider;home.mkdir()
        config=f'model_provider="{provider}"\nmodel="{model}"\ncli_auth_credentials_store="file"\n'
        if provider!='openai':
            config+='[model_providers.agentcockpit]\nname="Smoke"\nbase_url="http://127.0.0.1:1/v1"\nwire_api="responses"\n'
        (home/'config.toml').write_text(config,encoding='utf-8')
        run(home,fixture,provider,model)
