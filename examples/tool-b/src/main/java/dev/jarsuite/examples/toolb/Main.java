package dev.jarsuite.examples.toolb;

import dev.jarsuite.examples.commons.SharedStore;
import java.util.List;
import org.apache.commons.lang3.StringUtils;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class Main {
    private static final Logger LOG = LoggerFactory.getLogger(Main.class);

    public static void main(String[] args) throws Exception {
        if (args.length != 1 || !args[0].equals("list")) {
            System.err.println("usage: tool-b list");
            System.exit(64);
        }
        SharedStore store = SharedStore.open();
        LOG.info("commons {}, format {}, reading {}", SharedStore.VERSION, SharedStore.FORMAT, store.file());
        List<String> entries;
        try {
            entries = store.read();
        } catch (SharedStore.DataFormatException e) {
            System.err.println("tool-b: " + e.getMessage());
            System.exit(2);
            return;
        }
        for (int i = 0; i < entries.size(); i++) {
            System.out.println(StringUtils.leftPad(String.valueOf(i + 1), 3) + "  " + entries.get(i));
        }
        System.out.println("tool-b: " + entries.size() + " entries");
    }
}
