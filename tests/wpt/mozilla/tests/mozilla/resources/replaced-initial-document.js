// A document that replaces a frame's initial about:blank document and is same
// origin-domain with it reuses that document's window, which gets a new layout.
// https://html.spec.whatwg.org/multipage/#initialise-the-document-object (step 6)

function frameWithInitialDocument(t) {
  const frame = document.createElement("iframe");
  document.body.append(frame);
  t.add_cleanup(() => frame.remove());

  const initialDocument = frame.contentDocument;
  assert_equals(initialDocument.URL, "about:blank", "the frame shows its initial document");
  frame.contentWindow.initialWindowMarker = true;
  return { frame, initialDocument };
}

async function replaceInitialDocument(frame, srcdoc = "<p>Text</p>") {
  const initialDocument = frame.contentDocument;
  await new Promise(resolve => {
    frame.addEventListener("load", resolve, { once: true });
    frame.srcdoc = srcdoc;
  });

  const newDocument = frame.contentDocument;
  assert_not_equals(newDocument, initialDocument, "the frame shows a new document");
  assert_true(frame.contentWindow.initialWindowMarker, "the new document reuses the initial document's window");
  return newDocument;
}

// The color of the new document's paragraph after that document rebuilt its
// styles from every sheet its layout holds.
function colorOfParagraph(frame) {
  const document = frame.contentDocument;
  document.head.append(document.createElement("style"));
  const paragraph = document.querySelector("p");
  return frame.contentWindow.getComputedStyle(paragraph).color;
}
