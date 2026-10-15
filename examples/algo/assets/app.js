'use strict';
(function () {
  var stops = {
    deadline: 'the run passed its wall-clock deadline (app.toml: limits.deadline) and was stopped by the host',
    cancelled: 'the request was cancelled and the host stopped the run',
    heap: 'the run grew its heap past max_heap_words (app.toml) and its answer was discarded',
    host_calls: 'the run made more host calls than max_host_calls allows',
    call_depth: 'the run nested calls deeper than max_call_depth allows',
    queue_timeout: 'the request waited for a worker longer than its deadline and was never run',
    response_too_large: 'the answer was larger than max_response_bytes'
  };
  function el(tag, cls, text) { var e = document.createElement(tag); if (cls) e.className = cls; if (text) e.textContent = text; return e; }
  function meter(response) {
    var n = function (name) { var v = response.headers.get('x-cove-run-' + name); return v === null ? '–' : Number(v).toLocaleString('en-US'); };
    return 'instructions ' + n('instructions') + ' · yields ' + n('yields') + ' (declined ' + n('yields-declined') + ') · on a worker ' + n('worker-us') + ' µs · admission to answer ' + n('wall-us') + ' µs';
  }
  function show(target, node) { target.replaceChildren(node); }
  document.addEventListener('DOMContentLoaded', function () {
    var form = document.querySelector('form.run');
    var target = document.getElementById('result');
    if (!form || !target) return;
    var cancel = form.querySelector('button.cancel');
    var running = null;
    cancel.hidden = false;
    cancel.disabled = true;
    cancel.addEventListener('click', function () { if (running) running.abort(); });
    form.addEventListener('submit', function (event) {
      event.preventDefault();
      if (running) running.abort();
      var controller = new AbortController();
      running = controller;
      cancel.disabled = false;
      var body = new URLSearchParams(new FormData(form));
      body.set('part', 'result');
      var started = performance.now();
      show(target, el('div', 'running', 'Running… (Cancel withdraws the request; the host stops the run at its next safepoint)'));
      fetch(form.action, { method: 'POST', body: body, signal: controller.signal }).then(function (response) {
        return response.text().then(function (text) {
          var took = Math.round(performance.now() - started);
          if (response.ok) {
            target.innerHTML = text;
            var host = target.querySelector('[data-host]');
            if (host) host.textContent = meter(response) + ' · round trip ' + took + ' ms';
            return;
          }
          var stop = response.headers.get('x-cove-stop');
          var box = el('div', 'stop');
          box.dataset.stop = stop || String(response.status);
          box.appendChild(el('strong', null, stop ? 'Stopped: ' + stop + '. ' : 'Failed: HTTP ' + response.status + '. '));
          var why = stop ? (stops[stop] || 'the host stopped the run') : (response.status === 429 ? 'too many runs of this app are queued; try again in a moment' : '');
          box.appendChild(document.createTextNode(why ? why.charAt(0).toUpperCase() + why.slice(1) + '.' : ''));
          box.appendChild(el('p', 'host', meter(response) + ' · round trip ' + took + ' ms'));
          box.appendChild(el('pre', 'diag', text));
          show(target, box);
        });
      }).catch(function (error) {
        if (error.name === 'AbortError') {
          var box = el('div', 'stop');
          box.dataset.stop = 'cancelled';
          box.appendChild(el('strong', null, 'Cancelled. '));
          box.appendChild(document.createTextNode('You withdrew the request after ' + Math.round(performance.now() - started) + ' ms; the connection closed and the host cancelled the run.'));
          show(target, box);
        } else {
          show(target, el('div', 'stop', 'The request failed: ' + error));
        }
      }).finally(function () {
        if (running === controller) { running = null; cancel.disabled = true; }
      });
    });
    if (location.hash === '#run' || target.hasAttribute('data-autorun')) form.requestSubmit();
  });
})();
