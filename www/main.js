// Nautiloop — minimal JS (copy-to-clipboard + logo animation)

document.querySelectorAll('.copy-btn').forEach(btn => {
  btn.addEventListener('click', () => {
    const target = btn.dataset.copy || btn.closest('.hero-cta')?.querySelector('code')?.textContent?.replace(/^\$\s*/, '');
    if (!target) return;
    navigator.clipboard.writeText(target).then(() => {
      btn.classList.add('copied');
      btn.innerHTML = '&#10003;';
      setTimeout(() => {
        btn.classList.remove('copied');
        btn.innerHTML = '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="5" y="2" width="9" height="9" rx="1.5"/><path d="M2 6v7.5A1.5 1.5 0 003.5 15H11"/></svg>';
      }, 1500);
    });
  });
});

// Nautilus logo: single rotation on load (800ms, per DESIGN.md)
const logo = document.querySelector('.nav-brand svg');
if (logo) {
  logo.style.transition = 'transform 800ms ease-in-out';
  logo.style.transform = 'rotate(360deg)';
  setTimeout(() => { logo.style.transition = 'none'; }, 900);
}
