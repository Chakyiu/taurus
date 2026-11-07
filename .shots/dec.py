import json,base64,sys,glob,os
f=sorted(glob.glob(os.path.expanduser('~/.cursor/browser-logs/cdp-response-Page.captureScreenshot-*.json')))[-1]
d=json.load(open(f)); d=d.get('result',d)
open(sys.argv[1],'wb').write(base64.b64decode(d['data']))
