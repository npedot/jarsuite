package dev.jarsuite.examples.toola;

import dev.jarsuite.examples.commons.SharedStore;
import org.apache.commons.text.WordUtils;

public class Main {
    public static void main(String[] args) throws Exception {
        if (args.length < 2 || !args[0].equals("add")) {
            System.err.println("usage: tool-a add <text...>");
            System.exit(64);
        }
        SharedStore store = SharedStore.open();
        String entry = WordUtils.capitalizeFully(String.join(" ", java.util.Arrays.copyOfRange(args, 1, args.length)));
        try {
            store.append(entry);
        } catch (SharedStore.DataFormatException e) {
            System.err.println("tool-a: " + e.getMessage());
            System.exit(2);
        }
        System.out.printf("tool-a (commons %s, format %d): added \"%s\" to %s%n",
                SharedStore.VERSION, SharedStore.FORMAT, entry, store.file());
    }
}
