#!/usr/bin/env python3
"""Fork-only: writes tiny .docx, .xlsx, .pptx and .pdf files for the Mac test build's open check
and the live cloud test. Our own minimal documents, written from the published OOXML/PDF
structure; no third-party content. Usage: make-office-samples.py OUT_DIR"""
import os
import sys
import zipfile

OUT = sys.argv[1]
os.makedirs(OUT, exist_ok=True)

CT = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>{}</Types>'
OV = '<Override PartName="{}" ContentType="{}"/>'
RELS = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}</Relationships>'
REL = '<Relationship Id="{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{}" Target="{}"/>'
ML = "application/vnd.openxmlformats-officedocument."


def package(name, parts):
    with zipfile.ZipFile(os.path.join(OUT, name), "w", zipfile.ZIP_DEFLATED) as z:
        for path, body in parts:
            z.writestr(path, body)


package("sample.docx", [
    ("[Content_Types].xml", CT.format(OV.format("/word/document.xml", ML + "wordprocessingml.document.main+xml"))),
    ("_rels/.rels", RELS.format(REL.format("rId1", "officeDocument", "word/document.xml"))),
    ("word/document.xml", '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Craft cloud test document.</w:t></w:r></w:p></w:body></w:document>'),
])

package("sample.xlsx", [
    ("[Content_Types].xml", CT.format(OV.format("/xl/workbook.xml", ML + "spreadsheetml.sheet.main+xml") + OV.format("/xl/worksheets/sheet1.xml", ML + "spreadsheetml.worksheet+xml"))),
    ("_rels/.rels", RELS.format(REL.format("rId1", "officeDocument", "xl/workbook.xml"))),
    ("xl/_rels/workbook.xml.rels", RELS.format(REL.format("rId1", "worksheet", "worksheets/sheet1.xml"))),
    ("xl/workbook.xml", '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>'),
    ("xl/worksheets/sheet1.xml", '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Craft</t></is></c><c r="B1"><v>42</v></c></row></sheetData></worksheet>'),
])

P = 'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"'
TREE = '<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{}</p:spTree></p:cSld>'
TEXT = '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="457200" y="457200"/><a:ext cx="8229600" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>Craft cloud test slide</a:t></a:r></a:p></p:txBody></p:sp>'
CLR = '<a:{0}><a:srgbClr val="{1}"/></a:{0}>'
THEME = ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Plain"><a:themeElements><a:clrScheme name="Plain">'
         + "".join(CLR.format(n, v) for n, v in [("dk1", "000000"), ("lt1", "FFFFFF"), ("dk2", "333333"), ("lt2", "EEEEEE"), ("accent1", "4472C4"), ("accent2", "ED7D31"), ("accent3", "A5A5A5"), ("accent4", "FFC000"), ("accent5", "5B9BD5"), ("accent6", "70AD47"), ("hlink", "0563C1"), ("folHlink", "954F72")])
         + '</a:clrScheme><a:fontScheme name="Plain"><a:majorFont><a:latin typeface="Arial"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Arial"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme>'
         + '<a:fmtScheme name="Plain"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst>'
         + '<a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst>'
         + '<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>'
         + '<a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>')
package("sample.pptx", [
    ("[Content_Types].xml", CT.format(
        OV.format("/ppt/presentation.xml", ML + "presentationml.presentation.main+xml")
        + OV.format("/ppt/slides/slide1.xml", ML + "presentationml.slide+xml")
        + OV.format("/ppt/slideLayouts/slideLayout1.xml", ML + "presentationml.slideLayout+xml")
        + OV.format("/ppt/slideMasters/slideMaster1.xml", ML + "presentationml.slideMaster+xml")
        + OV.format("/ppt/theme/theme1.xml", ML + "theme+xml"))),
    ("_rels/.rels", RELS.format(REL.format("rId1", "officeDocument", "ppt/presentation.xml"))),
    ("ppt/_rels/presentation.xml.rels", RELS.format(REL.format("rId1", "slideMaster", "slideMasters/slideMaster1.xml") + REL.format("rId2", "slide", "slides/slide1.xml") + REL.format("rId3", "theme", "theme/theme1.xml"))),
    ("ppt/presentation.xml", f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<p:presentation {P}><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>'),
    ("ppt/slides/_rels/slide1.xml.rels", RELS.format(REL.format("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"))),
    ("ppt/slides/slide1.xml", f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<p:sld {P}>{TREE.format(TEXT)}</p:sld>'),
    ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", RELS.format(REL.format("rId1", "slideMaster", "../slideMasters/slideMaster1.xml"))),
    ("ppt/slideLayouts/slideLayout1.xml", f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<p:sldLayout {P} type="blank">{TREE.format("")}</p:sldLayout>'),
    ("ppt/slideMasters/_rels/slideMaster1.xml.rels", RELS.format(REL.format("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml") + REL.format("rId2", "theme", "../theme/theme1.xml"))),
    ("ppt/slideMasters/slideMaster1.xml", f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<p:sldMaster {P}>{TREE.format("")}<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>'),
    ("ppt/theme/theme1.xml", THEME),
])

# A one-page PDF with a line of text; the cross-reference offsets are computed, not guessed.
objs = [
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
    None,
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
]
stream = b"BT /F1 24 Tf 72 700 Td (Craft cloud test PDF) Tj ET"
objs[3] = b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"\nendstream"
out = bytearray(b"%PDF-1.4\n")
offsets = []
for i, body in enumerate(objs, 1):
    offsets.append(len(out))
    out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
xref = len(out)
out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
for off in offsets:
    out += b"%010d 00000 n \n" % off
out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
with open(os.path.join(OUT, "sample.pdf"), "wb") as f:
    f.write(out)
print("wrote", sorted(os.listdir(OUT)))
