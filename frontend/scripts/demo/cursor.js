/**
 * Draws a pointer into the page for the demo recording.
 *
 * Playwright's video capture does not include the OS cursor, so a click on the
 * record button is invisible in the finished GIF — the transport just changes
 * state on its own. This paints a pointer that follows the synthesized mouse
 * events, plus a ring on mousedown, so the viewer can see the button being
 * pressed.
 *
 * Injected only by scripts/demo/record-gif.mjs. It listens; it never moves the
 * pointer itself, so what shows up in the frame is exactly where the driver
 * clicked.
 */
(() => {
  const ARROW =
    'data:image/svg+xml;utf8,' +
    encodeURIComponent(
      `<svg xmlns="http://www.w3.org/2000/svg" width="22" height="30" viewBox="0 0 22 30">
         <path d="M2 1.6 L2 24.2 L7.9 18.6 L11.6 27.4 L15.1 25.9 L11.4 17.3 L19.3 17.0 Z"
               fill="#12100c" stroke="#ffffff" stroke-width="1.6" stroke-linejoin="round"/>
       </svg>`
    );

  const style = document.createElement('style');
  style.textContent = `
    #demo-cursor {
      position: fixed; top: 0; left: 0; width: 22px; height: 30px;
      z-index: 2147483647; pointer-events: none;
      background: url("${ARROW}") no-repeat center / contain;
      filter: drop-shadow(0 1px 2px rgba(0,0,0,.35));
      opacity: 0; transition: opacity .12s linear;
      will-change: transform;
    }
    #demo-cursor-ring {
      position: fixed; top: 0; left: 0; width: 34px; height: 34px; margin: -17px 0 0 -17px;
      border: 2px solid rgba(20,18,14,.55); border-radius: 50%;
      z-index: 2147483646; pointer-events: none; opacity: 0;
    }
    #demo-cursor-ring.demo-press { animation: demo-press .45s ease-out 1; }
    @keyframes demo-press {
      from { opacity: .9; transform: scale(.35); }
      to   { opacity: 0;  transform: scale(1.35); }
    }
  `;

  const install = () => {
    document.head.appendChild(style);

    const cursor = document.createElement('div');
    cursor.id = 'demo-cursor';
    const ring = document.createElement('div');
    ring.id = 'demo-cursor-ring';
    document.body.append(ring, cursor);

    // The pointer stays hidden until the driver first moves it, so the opening
    // frames are not stamped with a pointer parked at 0,0.
    addEventListener(
      'mousemove',
      (e) => {
        cursor.style.opacity = '1';
        cursor.style.transform = `translate(${e.clientX}px, ${e.clientY}px)`;
        ring.style.left = `${e.clientX}px`;
        ring.style.top = `${e.clientY}px`;
      },
      true
    );

    addEventListener(
      'mousedown',
      () => {
        ring.classList.remove('demo-press');
        // Force a reflow so the animation restarts on a second click.
        void ring.offsetWidth;
        ring.classList.add('demo-press');
      },
      true
    );
  };

  if (document.body) install();
  else addEventListener('DOMContentLoaded', install, { once: true });
})();
