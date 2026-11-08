;;;; Syntax highlighting
;;
;; tree-sitter names what each piece of code is (keyword, string,
;; function.method, ...). This says what each name looks like: a face, which
;; is a theme color (red green blue yellow magenta cyan) or default, strong,
;; dim, accent, error. A name not listed uses its parent's entry
;; (function.method -> function), else plain text.

(define (syntax-theme)
  (list (list "keyword" 'magenta) (list "operator" 'dim)
        (list "string" 'green) (list "string.special" 'cyan) (list "escape" 'cyan)
        (list "comment" 'dim)
        (list "function" 'blue) (list "function.macro" 'magenta) (list "constructor" 'yellow)
        (list "type" 'yellow) (list "type.builtin" 'yellow)
        (list "constant" 'cyan) (list "constant.builtin" 'cyan) (list "number" 'cyan)
        (list "boolean" 'cyan) (list "attribute" 'cyan) (list "label" 'cyan)
        (list "property" 'default) (list "variable.builtin" 'red)
        (list "punctuation" 'dim) (list "tag" 'red) (list "embedded" 'default)
        ;; Markdown (named by markdown.rs, styled by the md- faces below)
        (list "markup.heading.1" 'md-h1) (list "markup.heading.1.marker" 'md-h1-mark)
        (list "markup.heading.2" 'md-h2) (list "markup.heading.2.marker" 'md-h2-mark)
        (list "markup.heading.3" 'md-h3) (list "markup.heading.3.marker" 'md-h3-mark)
        (list "markup.heading.4" 'md-h4) (list "markup.heading.4.marker" 'md-h4-mark)
        (list "markup.heading.5" 'md-h5) (list "markup.heading.5.marker" 'md-h5-mark)
        (list "markup.heading.6" 'md-h6) (list "markup.heading.6.marker" 'md-h6-mark)
        (list "markup.bold" 'md-bold) (list "markup.italic" 'md-italic)
        (list "markup.raw" 'md-code) (list "markup.raw.block" 'md-code-block)
        (list "markup.link.text" 'md-link) (list "markup.link.url" 'md-url)
        (list "markup.quote" 'md-quote) (list "markup.quote.marker" 'md-quote)
        (list "markup.list" 'md-list) (list "markup.rule" 'md-rule)
        (list "markup.table.delimiter" 'dim) (list "markup.table.header" 'md-bold)))

;; How faces look beyond a color: (name key value ...). Keys: color,
;; background, scale (text size, 1 = normal), weight (bold), slant (italic),
;; underline (#t), family (sans: the UI font instead of the monospace one).
;; A face used in syntax-theme without a spec here is just a color.
(define (heading-faces level scale color)
  (let ((n (number->string level)))
    (list (list (string-append "md-h" n) 'color color 'scale scale 'weight 'bold 'family 'sans)
          (list (string-append "md-h" n "-mark") 'color 'dim 'scale scale 'family 'sans))))

(define (default-face-specs)
  (append
   (heading-faces 1 1.8 'accent)
   (heading-faces 2 1.5 'accent)
   (heading-faces 3 1.3 'blue)
   (heading-faces 4 1.15 'blue)
   (heading-faces 5 1.05 'default)
   (heading-faces 6 1.0 'default)
   (list (list "md-bold" 'weight 'bold)
         (list "md-italic" 'slant 'italic)
         (list "md-code" 'color 'cyan 'background 'muted)
         (list "md-code-block" 'color 'default 'background 'muted)
         (list "md-link" 'color 'blue 'underline #t)
         (list "md-url" 'color 'dim)
         (list "md-quote" 'color 'dim 'slant 'italic)
         (list "md-list" 'color 'accent 'weight 'bold)
         (list "md-rule" 'color 'dim))))

;; Your faces, in front of the defaults (the first spec with a name wins):
;; (set-faces! (list (list "md-bold" 'weight 'bold 'color 'yellow)))
(define custom-face-specs (list))
(define (set-faces! specs) (set! custom-face-specs (append specs custom-face-specs)))
(define (face-specs) (append custom-face-specs (default-face-specs)))

