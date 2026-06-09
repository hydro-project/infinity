const path = require("path");

module.exports = function pluginTranspileDeps() {
  return {
    name: "transpile-deps",
    configureWebpack(config) {
      const infinityUiPath = path.dirname(
        require.resolve("infinity-ui/package.json"),
      );
      const themePath = path.dirname(
        require.resolve("@hydro-project/docusaurus-theme/package.json"),
      );

      return {
        module: {
          rules: [
            {
              test: /\.tsx?$/,
              include: [
                path.join(infinityUiPath, "src"),
                path.join(themePath, "src"),
              ],
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
