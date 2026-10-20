// Highlight, in the chapter rail, the section or subsection being read, and keep it in view.
(function () {
  var nav = document.querySelector('nav.toc');
  var links = Array.prototype.slice.call(document.querySelectorAll('nav.toc .sub a'));
  var targets = links.map(function (a) { return document.getElementById(a.getAttribute('href').slice(1)); });
  if (!links.length) return;
  var current = null, queued = false;
  function update() {
    queued = false;
    var line = window.innerHeight * 0.25, pick = 0;
    targets.forEach(function (t, i) { if (t && t.getBoundingClientRect().top < line) pick = i; });
    if (current === links[pick]) return;
    if (current) current.classList.remove('on');
    current = links[pick];
    current.classList.add('on');
    // Only while the rail scrolls by itself (wide windows); on phones it sits above the text.
    if (getComputedStyle(nav).position === 'sticky') {
      var r = current.getBoundingClientRect(), n = nav.getBoundingClientRect();
      if (r.top < n.top + 40 || r.bottom > n.bottom - 40) nav.scrollTop += r.top - n.top - n.height / 3;
    }
  }
  window.addEventListener('scroll', function () { if (!queued) { queued = true; requestAnimationFrame(update); } }, { passive: true });
  window.addEventListener('resize', update);
  update();
})();
