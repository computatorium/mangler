// Class inheritance, super, static members, getters/setters, instanceof.
(function () {
  class Shape {
    static count = 0;
    constructor(name) {
      this.name = name;
      Shape.count++;
    }
    area() { return 0; }
    describe() { return `${this.name}:${this.area()}`; }
    static kind() { return "shape"; }
    get tag() { return `<${this.name}>`; }
    set tag(_) { this._ignored = true; }
    toString() { return `Shape(${this.name})`; }
  }
  class Rect extends Shape {
    constructor(w, h) {
      super("rect");
      this._w = w;
      this._h = h;
    }
    area() { return this._w * this._h; }
    describe() { return "R:" + super.describe(); }
  }
  const r = new Rect(3, 4);
  r.tag = "x"; // setter side effect
  globalThis.__out = JSON.stringify({
    describe: r.describe(),
    area: r.area(),
    tag: r.tag,
    ignored: r._ignored,
    str: String(r),
    count: Shape.count,
    kind: Shape.kind(),
    isRect: r instanceof Rect,
    isShape: r instanceof Shape,
  });
})();
