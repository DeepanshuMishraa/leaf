=================
Leaf Sample Guide
=================

:Author: Sample Writer
:Version: 1.2
:Date: 2026-11-28

This file exercises the reStructuredText reader. It has *emphasis*,
**strong text**, ``inline code``, a `named link <https://example.com>`_
and a reference to `Installing`_ below.

.. toctree::
   :maxdepth: 2

   this-directive-is-left-out

Installing
==========

Install with one command:

.. code-block:: bash

   cargo install --path . --locked
   leaf --version

A literal block made with a double colon::

   plain text, no colors
   kept exactly as written

.. note::

   Notes get a colored label, like GitHub alerts in markdown.

.. tip::

   Press ``q`` to quit.

.. warning::

   Unsaved edits are lost if you force quit.

Lists
=====

- First bullet
- Second bullet with a nested list:

  - Nested one
  - Nested two

- Third bullet

1. Numbered one
2. Numbered two
#. Auto-numbered three

Term one
   Definition of term one.

Term two
   Definition of term two.

Code
----

.. code-block:: python

   def greet(name: str) -> str:
       return f"Hello, {name}!"

   print(greet("leaf"))

.. code-block:: rust

   fn main() {
       println!("Hello from Rust");
   }

Tables
======

A grid table:

+------------+---------+--------+
| Format     | Reader  | Edits  |
+============+=========+========+
| CSV        | Grid    | Yes    |
+------------+---------+--------+
| XLSX       | Grid    | No     |
+------------+---------+--------+
| RST        | Document| No     |
+------------+---------+--------+

A simple table:

=====  =====  ======
  A      B    A or B
=====  =====  ======
False  False  False
True   False  True
False  True   True
True   True   True
=====  =====  ======

Images
======

.. image:: https://example.com/picture.png
   :alt: A sample picture

Quotes
======

    An indented paragraph becomes a block quote.

    -- Someone
