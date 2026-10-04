(() => {
  'use strict';
  const report = JSON.parse(document.getElementById('vault-report-data').textContent);
  const run = report.run || {};
  const metadata = report.metadata || {};
  const page = metadata.report_page || {};
  const results = Array.isArray(run.tests) ? run.tests : [];
  const executions = Array.isArray(report.executions) ? report.executions : [];
  const statuses = ['PASSED', 'FAILED', 'ERRORED', 'SKIPPED', 'NOT_RUN'];
  const labels = {PASSED:'Passed', FAILED:'Failed', ERRORED:'Errored', SKIPPED:'Skipped', NOT_RUN:'Not run'};
  const byId = id => document.getElementById(id);
  const array = value => Array.isArray(value) ? value : [];
  const object = value => value && typeof value === 'object' && !Array.isArray(value) ? value : {};
  const present = value => value !== undefined && value !== null && value !== '';
  const string = value => typeof value === 'string' ? value : present(value) ? JSON.stringify(value) : '';
  const node = (tag, cls, text) => { const el = document.createElement(tag); if (cls) el.className = cls; if (present(text)) el.textContent = string(text); return el; };
  const statusOf = value => statuses.includes(value) ? value : 'NOT_RUN';
  const badge = status => { const el = node('span', 'badge', labels[statusOf(status)]); el.dataset.status = statusOf(status); return el; };
  const number = value => Number.isFinite(Number(value)) ? Number(value) : 0;
  const duration = value => number(value) >= 1000 ? `${(number(value)/1000).toFixed(2)} s` : `${number(value)} ms`;
  const pretty = value => value && value._vault_preview === true ? string(value.preview) : typeof value === 'string' ? value : JSON.stringify(value, null, 2);
  const text = value => pretty(value) || '—';
  const readablePhase = phase => ({test:'Test',preparation:'Prepare test',reset:'Reset stores',seed:'Seed data',watch_snapshot:'Snapshot watched tables',watch_diff:'Check watched tables',mocks:'Arm mocks',arm_mocks:'Arm mocks',mock_arm:'Arm mocks',step:'HTTP step',http:'HTTP step',http_attempt:'HTTP attempt',capture:'Capture value',verify:'Verify end state',verify_store:'Verify store',verify_poll:'Verification poll',watch:'Verify watched state',mock_quiet:'Wait for outbound calls',calls:'Verify outbound calls',verify_calls:'Verify outbound calls',exports:'Export flow values',cleanup:'Finish test',report:'Finalize result',finalization:'Finish test'}[String(phase || '').toLowerCase()] || String(phase || 'Action').replace(/_/g,' '));
  const descriptors = array(metadata.tests).length ? metadata.tests.map((test, index) => ({...test, id:test.id || `test-${index}`, result_index:present(test.result_index) ? test.result_index : null, execution_index:present(test.execution_index) ? test.execution_index : null})) : results.map((test,index) => ({id:`test-${index}`,item_id:test.flow || `standalone-${index}`,name:test.name,flow:test.flow,stage_index:index,result_index:index,execution_index:index}));
  const resultFor = descriptor => present(descriptor.result_index) ? object(results[descriptor.result_index]) : {};
  const executionFor = descriptor => present(descriptor.execution_index) ? object(executions[descriptor.execution_index]) : {};
  const descriptorStatus = descriptor => statusOf(resultFor(descriptor).status);
  const worst = tests => {
    const values=tests.map(descriptorStatus);
    for (const status of ['ERRORED','FAILED','PASSED','NOT_RUN','SKIPPED']) if (values.includes(status)) return status;
    return 'NOT_RUN';
  };
  let expanded = false;
  const counts = tests => Object.fromEntries(statuses.map(status => [status,tests.filter(test => descriptorStatus(test) === status).length]));
  const totals = counts(descriptors);
  const runStatus = descriptors.length ? worst(descriptors) : 'NOT_RUN';
  const pageTitle = string(page.title);
  const summaryLabel = pageTitle ? (page.kind === 'flow' ? 'Whole flow' : 'Standalone test') : 'Whole run';
  byId('run-title').textContent = pageTitle || 'Test run';
  document.querySelector('.summary-caption').textContent = summaryLabel;
  document.querySelector('.summary').setAttribute('aria-label', `${summaryLabel} totals`);
  byId('run-context').textContent = string(metadata.suite) || 'Vault execution report';
  byId('run-status').replaceWith(Object.assign(badge(runStatus), {id:'run-status'}));
  document.title = pageTitle ? `Vault · ${pageTitle} · ${labels[runStatus]}` : `Vault · ${labels[runStatus]} test report`;
  const metadataRow = (name,value) => { if (!present(value)) return; const row = node('div'); row.append(node('dt',null,name), node('dd',null,value)); byId('run-metadata').append(row); };
  metadataRow('Environment',run.environment || 'Unknown');
  metadataRow('Started',metadata.started_at);
  metadataRow('Duration',duration(run.duration_ms));
  metadataRow('Selection',pageTitle || metadata.pattern || 'All tests');
  metadataRow('Tags',array(metadata.tags).join(', '));
  metadataRow('Shuffle seed',metadata.shuffle_seed);
  for (const status of statuses) {
    if (status === 'NOT_RUN' && totals[status] === 0) continue;
    const stat = node('div','summary-stat'); stat.dataset.status=status;
    stat.append(node('strong',null,totals[status]),node('span',null,labels[status])); byId('summary-counts').append(stat);
  }
  const allCount=node('div','summary-stat'); allCount.append(node('strong',null,descriptors.length),node('span',null,'Total tests')); byId('summary-counts').prepend(allCount);
  byId('report-version').textContent = ` / report schema ${report.schema_version || run.schema_version || 'unknown'}`;
  function previewNotice(value,omitted=0) {
    const bytes=value && value._vault_preview===true ? number(value.omitted_bytes) : number(omitted);
    if (!bytes) return null;
    const count=value && value.original_entries != null ? `; ${value.original_entries} original entries` : '';
    return node('p','truncation',`Preview truncated at 64 KiB; ${bytes.toLocaleString()} bytes omitted${count}.`);
  }
  function payload(parent,label,value) {
    if (!present(value)) return;
    const block=node('div','payload'); block.append(node('p','payload-label',label));
    let preview=text(value), truncated=0;
    if (value && value._vault_preview === true) truncated=number(value.omitted_bytes);
    else {
      const encoded=new TextEncoder().encode(preview);
      if (encoded.length>65536) { preview=new TextDecoder().decode(encoded.slice(0,65536)); truncated=encoded.length-65536; }
    }
    block.append(node('pre',null,preview));
    const notice=previewNotice(value,truncated); if (notice) block.append(notice);
    parent.append(block);
  }
  function differences(parent,diffs) {
    if (!array(diffs).length) return;
    const table=node('table','diff-table'); const head=node('thead'); const header=node('tr');
    for (const label of ['Field','Expected','Actual']) header.append(node('th',null,label)); head.append(header); table.append(head);
    const body=node('tbody');
    for (const diff of diffs) {
      const row=node('tr'); row.append(node('td',null,diff.path));
      for (const value of [diff.expected,diff.actual]) {
        const cell=node('td',null,text(value)); const notice=previewNotice(value); if(notice)cell.append(notice); row.append(cell);
      }
      body.append(row);
    }
    table.append(body); parent.append(table);
  }
  function exchangePayload(parent,label,value) {
    const exchange=object(value);
    if (!Object.prototype.hasOwnProperty.call(exchange,'body') && !Object.prototype.hasOwnProperty.call(exchange,'body_json')) { payload(parent,label,value); return; }
    const {body,body_json,headers,...metadata}=exchange;
    if (Object.keys(metadata).length) payload(parent,label,metadata);
    if (present(headers) && Object.keys(object(headers)).length) payload(parent,`${label} headers`,headers);
    if (present(body_json) && body_json!==null) payload(parent,`${label} body`,body_json);
    else if (present(body)) payload(parent,`${label} body`,body);
  }
  function checks(parent,value) {
    const list=Array.isArray(value) ? value : array(object(value).checks);
    if (!list.length) return;
    for (const check of list) {
      const section=node('div','check'); const failed=check.result==='fail';
      const heading=node('div','check-title'); heading.append(badge(failed?'FAILED':'PASSED'),node('span','check-description',check.description || 'Assertion')); section.append(heading);
      if (check.yaml_path) section.append(node('p','check-path',check.yaml_path));
      const kind=object(check.kind); differences(section,kind.diffs);
      if (failed && !array(kind.diffs).length && present(check.expected)) payload(section,'Expected',check.expected);
      for (const miss of array(check.near_misses)) {
        const details=node('details','evidence'); details.append(node('summary',null,`Closest observation · ${Math.round(number(miss.score)*100)}% match`)); const content=node('div','evidence-content'); differences(content,miss.diffs); payload(content,'Observed value',miss.actual); details.append(content); section.append(details);
      }
      if (failed && number(check.attempts)>1) section.append(node('p','muted',`${check.attempts} verification attempts over ${duration(check.elapsed_ms)}`));
      if (present(kind.actual)) payload(section,'Actual',kind.actual);
      if (present(kind.exchange)) payload(section,'Unexpected call',kind.exchange);
      if (present(kind.before)) payload(section,'Before the change',kind.before);
      if (present(kind.after)) payload(section,'After the change',kind.after);
      if (array(kind.interleaving).length || object(kind.interleaving)._vault_preview) payload(section,'Observed call order',kind.interleaving);
      parent.append(section);
    }
  }
  function recordings(parent,value) {
    if (value && value._vault_preview) { payload(parent,'Recorded calls',value); return; }
    for (const call of array(value)) {
      const row=node('div','call-row'); const request=object(call.request);
      row.append(node('div','call-title',`${present(call.seq)?`#${call.seq} `:''}${call.dependency || 'dependency'} · ${request.method || '?'} ${request.path || request.url || ''} · ${call.responded_status || '?'}`));
      if (call.matched_stub) row.append(node('div','muted',`Matched stub: ${call.matched_stub}`));
      const details=node('details','evidence'); details.append(node('summary',null,'Request details')); const content=node('div','evidence-content'); exchangePayload(content,'Request',request); details.append(content); row.append(details); parent.append(row);
    }
  }
  function evidence(parent,details) {
    const values=object(details);
    for (const [key,value] of Object.entries(values)) {
      if (!present(value) || key === 'error' || key === 'reason') continue;
      if (key === 'checks' || key === 'verify') checks(parent,value);
      else if (key === 'recorded_calls') recordings(parent,value);
      else if (key === 'request' || key === 'prepared_request' || key === 'response') exchangePayload(parent,key==='response'?'Response':'Request',value);
      else payload(parent,key.replace(/_/g,' ').replace(/^./,letter=>letter.toUpperCase()),value);
    }
  }
  function syntheticEvents(result) {
    return array(result.steps).map((step,index) => ({phase:'step',subject:step.name,status:step.status,attempts:step.attempts,step_index:index,duration_ms:object(step.response).elapsed_ms,details:{response:step.response,checks:step.checks}}));
  }
  function eventRow(event) {
    const status=statusOf(event.status); const row=node('li','event'); row.dataset.status=status; row.append(node('span','event-marker'));
    const card=node('div','event-card'); const heading=node('div','event-summary');
    let title=readablePhase(event.phase); if (event.subject && event.subject!==event.phase) title+=` · ${event.subject}`;
    heading.append(node('span','event-title',title),badge(status));
    const timings=[]; if (present(event.start_offset_ms)) timings.push(`+${duration(event.start_offset_ms)}`); if (present(event.duration_ms)) timings.push(duration(event.duration_ms)); if (number(event.attempts)>1) timings.push(`${event.attempts} attempts`);
    if (timings.length) heading.append(node('span','event-timing',timings.join(' / '))); card.append(heading);
    const data=object(event.details); const request=object(data.request || data.prepared_request); const requestPath=request.url || request.path;
    if (requestPath) card.append(node('p','event-request',`${request.method || ''} ${requestPath}`.trim()));
    if (data.error || data.reason) card.append(node('p','muted',data.error || data.reason));
    if (Object.keys(data).some(key=>key!=='error' && key!=='reason' && present(data[key]))) {
      const details=node('details','evidence'); details.open=status==='FAILED'||status==='ERRORED'; details.append(node('summary',null,'Evidence and payloads')); const content=node('div','evidence-content'); evidence(content,data); details.append(content); card.append(details);
    }
    row.append(card); return row;
  }
  function testCard(descriptor) {
    const result=resultFor(descriptor); const execution=executionFor(descriptor); const status=descriptorStatus(descriptor);
    const card=node('details','test'); card.id=`execution-${descriptors.indexOf(descriptor)}`; card.dataset.status=status; card.open=expanded || status==='FAILED' || status==='ERRORED';
    const summary=node('summary'); const title=node('div','test-heading'); title.append(node('div','test-name',descriptor.name || result.name || 'Unnamed test'));
    const sub=node('div','test-meta');
    if (present(descriptor.stage_index) && descriptor.flow) sub.append(document.createTextNode(`Stage ${number(descriptor.stage_index)+1} · `));
    if (descriptor.source) sub.append(document.createTextNode(string(descriptor.source)));
    for (const tag of array(descriptor.tags)) sub.append(node('span','tag',tag)); if (sub.childNodes.length) title.append(sub);
    summary.append(title,badge(status)); if (present(result.duration_ms)) summary.append(node('span','test-duration',duration(result.duration_ms))); card.append(summary);
    const body=node('div','test-body'); if (descriptor.description) body.append(node('p','test-description',descriptor.description));
    const preparation=array(execution.events).find(event=>event.phase==='preparation');
    if (descriptor.flow && preparation && object(preparation.details).reset_boundary===false) body.append(node('p','notice','Store state reused from an earlier flow stage.'));
    if (result.error) body.append(node('p','notice error',result.error)); if (result.skip_reason) body.append(node('p','notice',result.skip_reason)); if (status==='NOT_RUN') body.append(node('p','notice','This test was selected but was not executed.'));
    const events=array(execution.events).length?execution.events:syntheticEvents(result);
    const sectionTitle=node('div','section-heading'); sectionTitle.append(node('h3',null,'Execution lifecycle'),node('span',null,`${events.length} actions`)); body.append(sectionTitle);
    if (events.length) { const timeline=node('ol','lifecycle'); for (const event of events) timeline.append(eventRow(event)); body.append(timeline); }
    else body.append(node('p','muted',status==='SKIPPED'||status==='NOT_RUN'?'No actions executed.':'Lifecycle detail is unavailable for this result.'));
    if (array(object(result.verify).checks).length) { const heading=node('div','section-heading'); heading.append(node('h3',null,'End-state checks')); body.append(heading); checks(body,result.verify); }
    if (array(result.seed_receipts).length) payload(body,'Seed receipts',result.seed_receipts);
    if (Object.keys(object(result.captures)).length) payload(body,'Captured values',result.captures);
    if (array(result.recorded_calls).length || object(result.recorded_calls)._vault_preview) { const heading=node('div','section-heading'); heading.append(node('h3',null,'Outbound calls')); body.append(heading); recordings(body,result.recorded_calls); }
    if (Object.keys(object(execution.exports)).length) payload(body,'Flow exports',execution.exports);
    if (array(descriptor.export).length) payload(body,'Declared exports',descriptor.export);
    if (Object.keys(object(descriptor.with)).length) payload(body,'Stage inputs',descriptor.with);
    card.append(body); return card;
  }
  function searchable(descriptor) {
    const result=resultFor(descriptor); const execution=executionFor(descriptor);
    const item=object(items.get(string(descriptor.item_id)));
    const requests=array(execution.events).map(event=>{ const details=object(event.details); const request=object(details.request || details.prepared_request); return `${request.method || ''} ${request.url || request.path || ''}`; });
    return [descriptor.name,descriptor.flow,descriptor.description,descriptor.source,array(descriptor.tags).join(' '),item.name,item.description,array(item.tags).join(' '),result.name,...requests].filter(present).map(string).join(' ').toLowerCase();
  }
  const items=new Map(array(metadata.items).map(item=>[string(item.id),item]));
  const groups=new Map();
  for (const descriptor of descriptors) { const key=string(descriptor.item_id || descriptor.flow || descriptor.id); if (!groups.has(key)) groups.set(key,[]); groups.get(key).push(descriptor); }
  function render() {
    const query=byId('search-tests').value.trim().toLowerCase(); const filter=byId('status-filter').value;
    const matches=descriptor=>(filter==='ALL'||descriptorStatus(descriptor)===filter)&&(!query||searchable(descriptor).includes(query));
    const visible=descriptors.filter(matches); const visibleCounts=counts(visible);
    byId('filter-summary').textContent=`Showing ${visible.length} of ${descriptors.length} tests · ${statuses.filter(status=>visibleCounts[status]).map(status=>`${visibleCounts[status]} ${labels[status].toLowerCase()}`).join(', ') || 'No matches'}. Summary above shows whole-run totals.`;
    if (pageTitle) byId('filter-summary').textContent = byId('filter-summary').textContent.replace('whole-run totals', `${summaryLabel.toLowerCase()} totals`);
    const root=byId('results'); root.replaceChildren();
    for (const [key,tests] of groups) {
      const filtered=tests.filter(matches); if (!filtered.length) continue;
      const item=object(items.get(key)); const flow=item.kind==='flow' || tests.some(test=>test.flow); const group=node('section','result-group');
      const heading=node('div','group-heading'); const title=node('div','group-title'); title.append(node('h2',null,item.name || (flow?tests[0].flow:tests[0].name) || 'Tests'),node('span','group-kind',flow?'Flow':'Standalone test')); heading.append(title,badge(worst(tests))); group.append(heading);
      if (item.description) group.append(node('p','group-description',item.description));
      if (flow) {
        if (item.reset || item.on_failure) { const policy=node('p','flow-policy'); if (item.reset) policy.append(node('span',null,`Reset: ${item.reset}`)); if (item.on_failure) policy.append(node('span',null,`On failure: ${item.on_failure}`)); group.append(policy); }
        const rail=node('div','flow-strip'); rail.setAttribute('aria-label','Flow stages in execution order');
        tests.forEach((descriptor,index)=>{ if(index) rail.append(node('span','flow-connector')); const button=node('button','flow-node'); button.type='button'; button.dataset.status=descriptorStatus(descriptor); button.setAttribute('aria-label',`Open stage ${index+1}: ${descriptor.name}, ${labels[descriptorStatus(descriptor)]}`); button.append(node('span','stage-number',index+1),node('span','node-name',descriptor.name),badge(descriptorStatus(descriptor))); const stageResult=resultFor(descriptor); if(present(stageResult.duration_ms))button.append(node('span','node-duration',duration(stageResult.duration_ms))); button.addEventListener('click',()=>{ if(!matches(descriptor)){byId('search-tests').value='';byId('status-filter').value='ALL';render();} const card=byId(`execution-${descriptors.indexOf(descriptor)}`); if(card){card.open=true;card.scrollIntoView({block:'start',behavior:matchMedia('(prefers-reduced-motion: reduce)').matches?'auto':'smooth'});card.querySelector('summary').focus();} }); rail.append(button); }); group.append(rail);
      }
      for (const descriptor of filtered) group.append(testCard(descriptor)); root.append(group);
    }
    if (!visible.length) root.append(node('p','empty-state',descriptors.length?'No tests match these filters. Clear the search or select another status.':'No tests were recorded in this run.'));
    byId('expand-all').disabled=!visible.length;
  }
  byId('search-tests').addEventListener('input',render); byId('status-filter').addEventListener('change',render);
  byId('expand-all').addEventListener('click',()=>{ expanded=!expanded; byId('expand-all').textContent=expanded?'Collapse all':'Expand all'; for(const card of document.querySelectorAll('.test')) card.open=expanded; });
  byId('theme-toggle').addEventListener('click',()=>{ const dark=document.documentElement.dataset.theme!=='dark'; document.documentElement.dataset.theme=dark?'dark':'light'; byId('theme-toggle').textContent=dark?'Light theme':'Dark theme'; byId('theme-toggle').setAttribute('aria-label',dark?'Switch to light theme':'Switch to dark theme'); });
  if (matchMedia('(prefers-color-scheme: dark)').matches) {document.documentElement.dataset.theme='dark';byId('theme-toggle').textContent='Light theme';}
  const printState=[];
  window.addEventListener('beforeprint',()=>{printState.length=0;for(const details of document.querySelectorAll('details')){printState.push([details,details.open]);details.open=true;}});
  window.addEventListener('afterprint',()=>{for(const [details,open] of printState) details.open=open;});
  byId('print-report').addEventListener('click',()=>window.print());
  document.addEventListener('keydown',event=>{if(event.key==='/'&&!['INPUT','SELECT','TEXTAREA'].includes(document.activeElement.tagName)){event.preventDefault();byId('search-tests').focus();}if(event.key==='Escape'&&document.activeElement===byId('search-tests')){byId('search-tests').value='';render();}});
  render();
})();
