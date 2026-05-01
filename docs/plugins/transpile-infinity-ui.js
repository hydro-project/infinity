const path = require("path");

module.exports = function pluginTranspileInfinityUI() {
  return {
    name: "transpile-infinity-ui",
    configureWebpack(config) {
      const infinityUiPath = path.dirname(
        require.resolve("infinity-ui/package.json")
      );

      return {
        module: {
          rules: [
            {
              test: /\.tsx?$/,
              include: [path.join(infinityUiPath, "src")],
              use: [
                {
                  loader: require.resolve("babel-loader"),
                  options: {
                    presets: [
                      require.resolve("@babel/preset-typescript"),
                      [
                        require.resolve("@babel/preset-react"),
                        { runtime: "automatic" },
                      ],
                    ],
                  },
                },
              ],
            },
          ],
        },
        resolve: {
          alias: {
            "infinity-ui": path.join(infinityUiPath, "src"),
          },
          modules: [
            // Let webpack find infinity-ui's dependencies
            path.join(infinityUiPath, "node_modules"),
            "node_modules",
          ],
        },
      };
    },
  };
};
