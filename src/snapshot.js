(() => {
  function snapshotNode(node) {
    let text = "";

    if (node.localName === "script") {
      return [];
    }

    if (node.childNodes.length === 0) {
      text = node.innerText;
    }

    return [{
      element: node.localName ?? "",
      offset: (node.offsetLeft === undefined || node.offsetTop === undefined)
        ? undefined
        : [node.offsetLeft, node.offsetTop],
      size: (node.offsetWidth === undefined || node.offsetHeight === undefined)
        ? undefined
        : [node.offsetWidth, node.offsetHeight],
      attributes: Array.from(node.attributes ?? []).map((a) => ({
        [a.name]: a.value,
      })).reduce((a, d) => ({ ...a, ...d }), {}),
      children: Array.from(node.childNodes).flatMap(snapshotNode),
      text: text,
    }];
  }

  return snapshotNode(document.body)[0];
})();
