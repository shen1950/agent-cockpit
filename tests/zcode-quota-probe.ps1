$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Security
$dataRoot = Join-Path $env:USERPROFILE '.agent-cockpit'
$nativeRoot = Join-Path $env:USERPROFILE '.zcode'
$nativeFile = Join-Path $nativeRoot 'v2/credentials.json'
function Native-Hash {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return [Convert]::ToBase64String($sha.ComputeHash([IO.File]::ReadAllBytes($nativeFile))) } finally { $sha.Dispose() }
}
$before = Native-Hash
$vault = Get-Content -LiteralPath (Join-Path $dataRoot 'vault.json') -Raw | ConvertFrom-Json
if ($vault.has_password) { throw 'The diagnostic does not unlock password-protected vaults' }
$plain = [Security.Cryptography.ProtectedData]::Unprotect([Convert]::FromBase64String($vault.blob), $null, [Security.Cryptography.DataProtectionScope]::CurrentUser)
$map = [Text.Encoding]::UTF8.GetString($plain) | ConvertFrom-Json
$accounts = @($map.PSObject.Properties | Where-Object { $_.Name -like 'zcode-login:*' } | ForEach-Object { $_.Value | ConvertFrom-Json })
if (-not $accounts.Count) { throw 'No registered ZCode login snapshots' }
$payload = @{ native_root = $nativeRoot; accounts = $accounts } | ConvertTo-Json -Depth 24 -Compress
$script = @'
import base64, hashlib, json, os, pathlib, sys, urllib.request, urllib.error
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
payload=json.loads(sys.stdin.buffer.read().decode('utf-8-sig'))
secret=os.environ.get('ZCODE_CREDENTIAL_SECRET') or 'zcode-credential-fallback:%s:%s:%s'%(sys.platform,pathlib.Path.home(),os.environ.get('USERNAME') or os.environ.get('USER') or 'unknown')
key=hashlib.sha256(secret.encode()).digest()
def dec(s):
 if not s.startswith('enc:v1:'):return s
 iv,tag,ct=[base64.urlsafe_b64decode(p+'='*(-len(p)%4)) for p in s[7:].split('.')]
 return AESGCM(key).decrypt(iv,ct+tag,None).decode('utf-8')
class NoRedirect(urllib.request.HTTPRedirectHandler):
 def redirect_request(self,*args,**kwargs):return None
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}),NoRedirect())
telemetry=json.loads((pathlib.Path(payload['native_root'])/'v2/telemetry-state.json').read_text(encoding='utf-8'))
for snapshot in payload['accounts']:
 who=snapshot['identity']; jwt=dec(snapshot['record']['zcodejwttoken']); parts=jwt.split('.')
 claims=json.loads(base64.urlsafe_b64decode(parts[1]+'='*(-len(parts[1])%4)))
 for version in (['3.14.5','3.16.3'] if who['provider']=='bigmodel' else ['3.16.3']):
  origin='https://zcode.z.ai';path='/api/v1/zcode-plan/billing/balance?app_version='+version
  request=urllib.request.Request(origin+path,headers={'Accept':'application/json','Authorization':'Bearer '+jwt,'User-Agent':'ZCode/'+version,'HTTP-Referer':origin,'X-Title':'Z Code@electron','X-ZCode-App-Version':version,'X-Device-Mid':telemetry['deviceMid']})
  result={'provider':who['provider'],'app_version':version,'path':path,'jwt_user_matches_snapshot':str(claims.get('user_id') or claims.get('sub') or '')==who['provider_user_id'],'jwt_iat':claims.get('iat'),'jwt_exp':claims.get('exp')}
  try:
   with opener.open(request,timeout=18) as response:
    result['status']=response.status;result['response_date']=response.headers.get('date');body=json.loads(response.read().decode('utf-8'))
   data=body.get('data') or {};balances=data.get('balances');plans=data.get('plans')
   result.update({'code':body.get('code'),'body_keys':sorted(body),'data_keys':sorted(data),'balances_type':type(balances).__name__,'plans_type':type(plans).__name__,'balance_count':len(balances) if isinstance(balances,list) else None,'plan_count':len(plans) if isinstance(plans,list) else None,'server_time':data.get('server_time')})
   if isinstance(balances,list):result['balances']=[{k:b.get(k) for k in ['show_name','total_units','used_units','remaining_units','available_units','expires_at']} for b in balances]
   if isinstance(plans,list):result['plans']=[{k:p.get(k) for k in ['name','status','starts_at','ends_at']} for p in plans]
  except urllib.error.HTTPError as error:result.update({'status':error.code,'exception':'HTTPError'})
  except Exception as error:result['exception']=type(error).__name__
  print(json.dumps(result,ensure_ascii=False))
'@
$payload | & (Join-Path $env:USERPROFILE '.agenthub/venv/Scripts/python.exe') -X utf8 -c $script
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
if ((Native-Hash) -ne $before) { throw 'Native credentials changed during diagnostic' }
Write-Output 'native_credentials_unchanged=true no_refresh_or_switch_performed=true'
