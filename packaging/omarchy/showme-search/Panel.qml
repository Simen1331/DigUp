import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// ShowMe's search in the bar: a field-like pill that opens a search field with the hits under it as you type.
// The bar itself takes no keys, so the field lives in a KeyboardPanel dropped from the pill. Enter opens the hit
// where it matched (the page, the moment), Shift+Enter shows all of them in ShowMe's window, Esc closes.
Panel {
  id: root
  moduleName: "showme.search"
  ipcTarget: "showme.search"
  manageIpc: false

  property var hits: []
  property int selectedIndex: 0
  // The query the running search is for, and whether one is running.
  property string searchedQuery: ""
  property bool searching: false
  property string problem: ""

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property string query: field.text.trim()

  implicitWidth: pill.implicitWidth
  // The bar's full height, so the pill sits in its middle.
  implicitHeight: Style.bar.sizeHorizontal

  onOpenedChanged: {
    if (opened) {
      field.selectAll()
      Qt.callLater(function() { field.forceActiveFocus() })
    }
  }

  function search() {
    if (query === "") {
      hits = []
      problem = ""
      return
    }
    if (searching) return
    searching = true
    searchedQuery = query
    searchProcess.command = ["bash", "-lc", 'exec showme search --json -n 8 -- "$1"', "showme", query]
    searchProcess.running = true
  }

  function finished(text) {
    searching = false
    try {
      hits = JSON.parse(text)
      problem = hits.length === 0 ? "Nothing found" : ""
    } catch (error) {
      hits = []
      problem = "ShowMe isn't answering (is the daemon running?)"
    }
    selectedIndex = 0
    // The field changed while it searched: search again for what it says now.
    if (query !== searchedQuery) search()
  }

  function openHit(index) {
    if (index < 0 || index >= hits.length) return
    Quickshell.execDetached(["bash", "-lc", 'exec showme-panel --open "$1" "$2"', "showme", JSON.stringify(hits[index]), query])
    close()
  }

  function openWindow() {
    Quickshell.execDetached(["bash", "-lc", 'exec showme-panel --window "$1"', "showme", query])
    close()
  }

  function fileName(path) { return path.substring(path.lastIndexOf("/") + 1) }

  function folder(path) {
    var home = Quickshell.env("HOME")
    var parent = path.substring(0, path.lastIndexOf("/"))
    return parent.indexOf(home) === 0 ? "~" + parent.substring(home.length) : parent
  }

  function clock(seconds) {
    var whole = Math.floor(seconds)
    var minutes = Math.floor(whole / 60)
    var rest = whole % 60
    return minutes + ":" + (rest < 10 ? "0" : "") + rest
  }

  // Where in the file it matched: "p. 4", "1:23".
  function place(hit) {
    if (hit.loc === undefined || hit.loc === null) return ""
    if (hit.segment === "page" || hit.segment === "page_image") return "p. " + Math.round(hit.loc)
    if (hit.segment === "frame" || hit.segment === "audio") return clock(hit.loc)
    return ""
  }

  function icon(kind) {
    switch (kind) {
      case "pdf": return "󰈦"
      case "image": return "󰋩"
      case "screenshot": return "󰹑"
      case "video": return "󰕧"
      case "audio": return "󰎆"
      case "code": return "󰅩"
      default: return "󰈙"
    }
  }

  // The excerpt with the words that matched (marked « ») in bold.
  function marked(text) {
    var escaped = String(text || "").replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    return escaped.replace(/«/g, "<b>").replace(/»/g, "</b>")
  }

  Process {
    id: searchProcess
    stdout: StdioCollector {
      onStreamFinished: root.finished(text)
    }
  }

  Timer {
    id: typing
    interval: 180
    onTriggered: root.search()
  }

  // The pill: looks like the field it opens.
  Rectangle {
    id: pill
    anchors.verticalCenter: parent.verticalCenter
    implicitWidth: Style.space(220)
    implicitHeight: Style.space(18)
    radius: Style.cornerRadius
    color: pillArea.containsMouse || root.opened ? Style.hoverFillFor(root.foreground, Color.accent) : "transparent"
    border.width: 1
    border.color: root.opened ? Color.accent : Qt.darker(root.foreground, 2.2)

    Row {
      anchors.left: parent.left
      anchors.leftMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.space(6)

      Text {
        text: "󰍉"
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        anchors.verticalCenter: parent.verticalCenter
      }
      Text {
        textFormat: Text.PlainText
        text: root.query !== "" ? root.query : "Search your files"
        color: root.dim
        elide: Text.ElideRight
        width: pill.width - Style.space(40)
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
        anchors.verticalCenter: parent.verticalCenter
      }
    }

    MouseArea {
      id: pillArea
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onClicked: root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: pill
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: field
    contentWidth: panel.fittedContentWidth(Style.space(560))
    contentHeight: panel.fittedContentHeight(content.implicitHeight, Style.space(620))

    Column {
      id: content
      width: parent.width
      spacing: Style.space(8)

      TextField {
        id: field
        width: parent.width
        placeholderText: "Describe what you're looking for"
        foreground: root.foreground
        font.family: root.fontFamily

        onTextChanged: typing.restart()

        Keys.onPressed: function(event) {
          if (event.key === Qt.Key_Escape) {
            root.close()
            event.accepted = true
          } else if (event.key === Qt.Key_Down) {
            if (root.selectedIndex < root.hits.length - 1) root.selectedIndex++
            event.accepted = true
          } else if (event.key === Qt.Key_Up) {
            if (root.selectedIndex > 0) root.selectedIndex--
            event.accepted = true
          } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
            if (event.modifiers & Qt.ShiftModifier) root.openWindow()
            else root.openHit(root.selectedIndex)
            event.accepted = true
          }
        }
      }

      Text {
        visible: text !== ""
        text: root.problem
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.body
      }

      Repeater {
        model: root.hits

        delegate: Rectangle {
          id: row
          required property var modelData
          required property int index

          width: content.width
          height: rowColumn.implicitHeight + Style.space(12)
          radius: Style.cornerRadius
          color: index === root.selectedIndex || rowArea.containsMouse
            ? Style.hoverFillFor(root.foreground, Color.accent) : "transparent"

          Text {
            id: rowIcon
            text: root.icon(row.modelData.kind)
            color: index === root.selectedIndex ? Color.accent : root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.title
            anchors.left: parent.left
            anchors.leftMargin: Style.space(8)
            anchors.top: parent.top
            anchors.topMargin: Style.space(7)
          }

          Column {
            id: rowColumn
            anchors.left: rowIcon.right
            anchors.leftMargin: Style.space(10)
            anchors.right: parent.right
            anchors.rightMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)

            Text {
              width: parent.width
              textFormat: Text.PlainText
              text: root.fileName(row.modelData.path)
              color: root.foreground
              elide: Text.ElideMiddle
              font.family: root.fontFamily
              font.pixelSize: Style.font.body
              font.bold: true
            }
            Text {
              width: parent.width
              textFormat: Text.PlainText
              text: [root.folder(row.modelData.path), root.place(row.modelData)].filter(function(part) { return part !== "" }).join("  ·  ")
              color: root.dim
              elide: Text.ElideLeft
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }
            Text {
              width: parent.width
              visible: !!row.modelData.excerpt
              textFormat: Text.StyledText
              text: root.marked(row.modelData.excerpt)
              color: root.dim
              wrapMode: Text.Wrap
              maximumLineCount: 2
              elide: Text.ElideRight
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }
          }

          MouseArea {
            id: rowArea
            anchors.fill: parent
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onClicked: root.openHit(row.index)
          }
        }
      }

      Text {
        visible: root.hits.length > 0
        text: "Enter open  ·  Shift+Enter all results in ShowMe  ·  Esc close"
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
      }
    }
  }
}
