'use strict';
// The admin pages' script, which the host answers from assets/ so that the
// pages' CSP can say script-src 'self'. It is what two attributes of the
// markup do:
// - data-confirm="<question>" on a button: the click goes ahead only if
//   confirm() is answered yes, so a form is not submitted on a no;
// - data-copy="<id>" on a button: the text of the element with that id is
//   copied to the clipboard.
document.addEventListener('click', function (event) {
  var target = event.target instanceof Element ? event.target : null;
  if (target === null) return;
  var asking = target.closest('[data-confirm]');
  if (asking !== null && !confirm(asking.getAttribute('data-confirm'))) {
    event.preventDefault();
    return;
  }
  var copying = target.closest('[data-copy]');
  if (copying !== null) {
    var source = document.getElementById(copying.getAttribute('data-copy'));
    if (source !== null) navigator.clipboard.writeText(source.textContent);
  }
});
