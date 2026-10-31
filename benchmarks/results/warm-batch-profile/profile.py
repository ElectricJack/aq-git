import importlib.util,pathlib,json,time,threading,signal,os
root=pathlib.Path.cwd(); spec=importlib.util.spec_from_file_location('bench',root/'benchmarks/run.py'); b=importlib.util.module_from_spec(spec);spec.loader.exec_module(b)
out=root/'.local/batch-profile'; queries=json.load(open(root/'benchmarks/results/final-audited-warm/queries.json')); sock=pathlib.Path('/tmp/aq-batch-profile.sock'); state=pathlib.Path('/tmp/aq-git-benchmark/state-20261031T012210Z')
log=open(out/'service.log','w'); p=b.start_server(root/'target/release/aq-git',state,sock,log); client=b.Client(sock)
stop=threading.Event(); rows=[]
def sample():
 while not stop.is_set():
  entry={'time':time.monotonic(),'processes':[]}
  try:
   children=pathlib.Path(f'/proc/{p.pid}/task/{p.pid}/children').read_text().split()
   for pid in [str(p.pid)]+children:
    status=pathlib.Path(f'/proc/{pid}/status').read_text(); fields={line.split(':')[0]:line.split(':',1)[1].strip() for line in status.splitlines() if ':' in line}
    threads=[]
    for task in pathlib.Path(f'/proc/{pid}/task').iterdir():
     st=(task/'stat').read_text().split(); threads.append({'tid':task.name,'name':(task/'comm').read_text().strip(),'ticks':int(st[13])+int(st[14]),'wchan':(task/'wchan').read_text().strip()})
    entry['processes'].append({'pid':pid,'fields':{k:fields.get(k) for k in ['VmRSS','RssAnon','VmData','Threads']},'threads':threads})
   entry['pressure']=pathlib.Path('/proc/pressure/cpu').read_text()
  except (OSError,ValueError):pass
  rows.append(entry);stop.wait(1)
t=threading.Thread(target=sample);t.start()
try:
 for q in queries:client.execute(q)
 before=client.execute(b.request('service.metrics',{}))
 result=b.measure('service',queries,{},sock,32,60,42,out/'samples.csv',batch=True,service_pid=p.pid)
 after=client.execute(b.request('service.metrics',{}))
 json.dump({'result':result,'before':before,'after':after},open(out/'result.json','w'),indent=2)
finally:
 stop.set();t.join();json.dump(rows,open(out/'threads.json','w'));client.close();p.send_signal(signal.SIGTERM);p.wait(timeout=30);log.close()
