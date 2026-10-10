// Apache Guacamole's own guacamole-common (the library the Guacamole web application uses
// to speak to guacd) as a client of NetGet's Guacamole server. One JSON object per line.
//
//   java GuacPeer <host> <port> <username>
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import org.apache.guacamole.GuacamoleException;
import org.apache.guacamole.io.GuacamoleReader;
import org.apache.guacamole.io.GuacamoleWriter;
import org.apache.guacamole.net.InetGuacamoleSocket;
import org.apache.guacamole.net.GuacamoleSocket;
import org.apache.guacamole.protocol.ConfiguredGuacamoleSocket;
import org.apache.guacamole.protocol.GuacamoleClientInformation;
import org.apache.guacamole.protocol.GuacamoleConfiguration;
import org.apache.guacamole.protocol.GuacamoleInstruction;

public class GuacPeer {
    static String q(String s) {
        StringBuilder b = new StringBuilder("\"");
        for (char c : s.toCharArray()) {
            if (c == '"' || c == '\\') b.append('\\').append(c);
            else if (c < 0x20) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.append('"').toString();
    }

    static void out(String json) { System.out.println(json); System.out.flush(); }

    /** Read until a sync, collecting what the frame drew; answer the sync. */
    static List<GuacamoleInstruction> frame(GuacamoleReader r, GuacamoleWriter w) throws GuacamoleException {
        List<GuacamoleInstruction> got = new ArrayList<>();
        while (true) {
            GuacamoleInstruction i = r.readInstruction();
            if (i == null) throw new RuntimeException("closed");
            if (i.getOpcode().equals("sync")) {
                w.writeInstruction(new GuacamoleInstruction("sync", i.getArgs().get(0)));
                if (!got.isEmpty()) return got;
                continue;
            }
            got.add(i);
        }
    }

    static String describe(List<GuacamoleInstruction> frame) {
        StringBuilder ops = new StringBuilder("[");
        StringBuilder clip = new StringBuilder();
        int pngs = 0;
        StringBuilder png = new StringBuilder();
        for (GuacamoleInstruction i : frame) {
            if (ops.length() > 1) ops.append(',');
            ops.append(q(i.getOpcode()));
            if (i.getOpcode().equals("blob")) png.append(i.getArgs().get(1));
            if (i.getOpcode().equals("end")) {
                byte[] b = Base64.getDecoder().decode(png.toString());
                if (b.length > 8 && b[1] == 'P' && b[2] == 'N' && b[3] == 'G') {
                    pngs++;
                } else {
                    clip.append(new String(b, java.nio.charset.StandardCharsets.UTF_8));
                }
                png.setLength(0);
            }
        }
        return ops.append("], \"pngs\": ").append(pngs).append(", \"clipboard\": ").append(q(clip.toString())).toString();
    }

    public static void main(String[] a) throws Exception {
        GuacamoleConfiguration config = new GuacamoleConfiguration();
        config.setProtocol("vnc");
        config.setParameter("hostname", "desktop.example");
        config.setParameter("username", a[2]);
        config.setParameter("password", "s3cret");
        GuacamoleClientInformation info = new GuacamoleClientInformation();
        info.setOptimalScreenWidth(640);
        info.setOptimalScreenHeight(480);
        info.getImageMimetypes().add("image/png");
        info.setTimezone("Europe/Bratislava");
        GuacamoleSocket socket;
        try {
            socket = new ConfiguredGuacamoleSocket(new InetGuacamoleSocket(a[0], Integer.parseInt(a[1])), config, info);
        } catch (GuacamoleException e) {
            out("{\"step\": \"refused\", \"exception\": " + q(e.getClass().getSimpleName()) + ", \"message\": " + q(e.getMessage()) + "}");
            return;
        }
        String id = ((ConfiguredGuacamoleSocket) socket).getConnectionID();
        GuacamoleReader r = socket.getReader();
        GuacamoleWriter w = socket.getWriter();
        out("{\"step\": \"ready\", \"id\": " + q(id) + ", \"version\": " + q(String.valueOf(((ConfiguredGuacamoleSocket) socket).getProtocolVersion())) + "}");
        out("{\"step\": \"first\", \"ops\": " + describe(frame(r, w)) + "}");
        // Type a line.
        for (char c : "java".toCharArray()) {
            w.writeInstruction(new GuacamoleInstruction("key", String.valueOf((int) c), "1"));
            w.writeInstruction(new GuacamoleInstruction("key", String.valueOf((int) c), "0"));
        }
        w.writeInstruction(new GuacamoleInstruction("key", String.valueOf(0xff0d), "1"));
        w.writeInstruction(new GuacamoleInstruction("key", String.valueOf(0xff0d), "0"));
        out("{\"step\": \"typed\", \"ops\": " + describe(frame(r, w)) + "}");
        // Send the clipboard.
        w.writeInstruction(new GuacamoleInstruction("clipboard", "3", "text/plain"));
        w.writeInstruction(new GuacamoleInstruction("blob", "3", Base64.getEncoder().encodeToString("from java".getBytes())));
        w.writeInstruction(new GuacamoleInstruction("end", "3"));
        out("{\"step\": \"clipboard\", \"ops\": " + describe(frame(r, w)) + "}");
        w.writeInstruction(new GuacamoleInstruction("disconnect"));
        socket.close();
    }
}
