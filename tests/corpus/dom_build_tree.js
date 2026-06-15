// DOM-shaped recorder: createElement/setAttribute/appendChild/textContent.
// Self-contained fake document; asserts the DOM op SEQUENCE is preserved.
(function () {
  const __ops = [];
  function makeEl(tag) {
    return {
      tagName: tag,
      children: [],
      attrs: {},
      _text: "",
      setAttribute(k, v) {
        __ops.push(["setAttribute", this.tagName, k, v]);
        this.attrs[k] = v;
      },
      appendChild(child) {
        __ops.push(["appendChild", this.tagName, child.tagName]);
        this.children.push(child);
        return child;
      },
      set textContent(v) {
        __ops.push(["textContent", this.tagName, v]);
        this._text = v;
      },
      get textContent() {
        return this._text;
      },
    };
  }
  const document = {
    createElement(tag) {
      __ops.push(["createElement", tag]);
      return makeEl(tag);
    },
  };

  const root = document.createElement("div");
  root.setAttribute("id", "app");
  const list = document.createElement("ul");
  root.appendChild(list);
  for (let i = 0; i < 3; i++) {
    const li = document.createElement("li");
    li.setAttribute("data-index", String(i));
    li.textContent = `item ${i}`;
    list.appendChild(li);
  }
  const footer = document.createElement("footer");
  footer.textContent = "done";
  root.appendChild(footer);

  globalThis.__out = JSON.stringify(__ops);
})();
